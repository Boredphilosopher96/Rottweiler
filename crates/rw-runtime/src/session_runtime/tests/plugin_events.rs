use super::Arc;
use super::BTreeSet;
use super::BlockedPluginEventPublisher;
use super::EngineEvent;
use super::EventMeta;
use super::FailingPluginEventPublisher;
use super::FixtureRedactor;
use super::Ordering;
use super::PLUGIN_EVENT_QUEUE_CAPACITY;
use super::PLUGIN_EVENT_SUSTAINED_OVERFLOW;
use super::PluginFanoutWorker;
use super::SESSION_EVENT_VERSION;
use super::SequenceId;
use super::SessionId;
use super::SharedEngineSecretRedactor;
use super::credential_shaped_environment_name;
use super::plugin_event_payload;
use super::register_credential_environment_value;

#[test]
fn credential_shaped_environment_values_join_the_shared_redaction_set() {
    let redactor = FixtureRedactor::default();
    for (name, value) in [
        ("OPENAI_API_KEY", "api-canary"),
        ("MY_TOKEN", "token-canary"),
        ("SERVICE_SECRET", "secret-canary"),
        ("DB_PASSWORD", "password-canary"),
        ("SIGNING_PRIVATE_KEY", "private-key-canary"),
        ("NORMAL_SETTING", "visible-canary"),
        ("EMPTY_TOKEN", ""),
    ] {
        register_credential_environment_value(&redactor, name, value);
    }
    let redacted = redactor.redact_text(
        "api-canary token-canary secret-canary password-canary private-key-canary visible-canary",
    );
    for secret in [
        "api-canary",
        "token-canary",
        "secret-canary",
        "password-canary",
        "private-key-canary",
    ] {
        assert!(!redacted.contains(secret));
    }
    assert!(redacted.contains("visible-canary"));
    assert!(!credential_shaped_environment_name("MAX_TOKENS"));
    assert!(!credential_shaped_environment_name("TOKEN_COUNT"));
}

#[test]
fn plugin_event_fanout_uses_canonical_names_and_redacts_payloads() {
    let redactor = FixtureRedactor::new(["fanout-secret-canary".to_owned()]);
    let event = EngineEvent::PluginStatusChanged {
        meta: EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: SessionId("fixture-session".to_owned()),
            sequence_id: SequenceId(4),
            emitted_at: "2026-07-11T00:00:00Z".to_owned(),
            caused_by: None,
        },
        plugin_id: "fixture-plugin".to_owned(),
        status: "working fanout-secret-canary".to_owned(),
    };
    let (wire_name, manifest_name, payload) =
        plugin_event_payload(&redactor, &event).expect("fanout payload");
    assert_eq!(wire_name, "plugin_status_changed");
    assert_eq!(manifest_name, "PluginStatusChanged");
    let encoded = serde_json::to_string(&payload).expect("encoded payload");
    assert!(!encoded.contains("fanout-secret-canary"));
    assert!(encoded.contains("[REDACTED]"));
}

#[tokio::test]
async fn plugin_event_fanout_is_nonblocking_bounded_and_disables_sustained_overflow() {
    let worker = PluginFanoutWorker::new(
        BTreeSet::from(["TextDelta".to_owned()]),
        Arc::new(BlockedPluginEventPublisher),
    );
    let started = std::time::Instant::now();
    // Fill the bounded queue, then cross the exact sustained-overflow
    // threshold. Tens of thousands of JSON allocations only benchmarked a
    // debug build and made this logical non-blocking regression host-load
    // dependent without exercising another state transition.
    for index in 0..=(PLUGIN_EVENT_QUEUE_CAPACITY + PLUGIN_EVENT_SUSTAINED_OVERFLOW) {
        worker.publish(
            "text_delta",
            "TextDelta",
            serde_json::json!({"type":"text_delta","index":index}),
        );
    }
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "fanout producer blocked on a stalled plugin"
    );
    assert!(worker.disabled.load(Ordering::Acquire));
    assert!(
        worker.overflow.load(Ordering::Acquire) >= PLUGIN_EVENT_SUSTAINED_OVERFLOW,
        "sustained overflow was not accounted"
    );
    assert!(worker.sender.capacity() <= PLUGIN_EVENT_QUEUE_CAPACITY);
}

#[tokio::test]
async fn plugin_event_fanout_disables_sustained_rpc_failures() {
    let worker = PluginFanoutWorker::new(
        BTreeSet::from(["TextDelta".to_owned()]),
        Arc::new(FailingPluginEventPublisher),
    );
    for index in 0..PLUGIN_EVENT_SUSTAINED_OVERFLOW {
        worker.publish(
            "text_delta",
            "TextDelta",
            serde_json::json!({"type":"text_delta","index":index}),
        );
        tokio::task::yield_now().await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !worker.disabled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failing plugin must be disabled");
    assert!(
        worker.overflow.load(Ordering::Acquire) >= PLUGIN_EVENT_SUSTAINED_OVERFLOW,
        "sustained delivery failures were not accounted"
    );
}

#[test]
fn engine_stream_redactor_holds_every_supported_private_key_envelope() {
    let redactor = SharedEngineSecretRedactor(FixtureRedactor::default());
    let incomplete = "prefix\n-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n-----END harmless";
    assert!(rw_core::SecretRedactor::has_incomplete_secret_envelope(
        &redactor, incomplete,
    ));
    let complete = format!("{incomplete}\n-----END OPENSSH PRIVATE KEY-----\nsuffix");
    assert!(!rw_core::SecretRedactor::has_incomplete_secret_envelope(
        &redactor, &complete,
    ));
}
