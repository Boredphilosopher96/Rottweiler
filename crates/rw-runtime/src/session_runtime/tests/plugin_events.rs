#![cfg(test)]
use super::EngineEvent;
use super::EventMeta;
use super::FixtureRedactor;
use super::SESSION_EVENT_VERSION;
use super::SequenceId;
use super::SessionId;
use super::SharedEngineSecretRedactor;
use super::credential_shaped_environment_name;
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

#[test]
fn namespace_transactions_never_enter_another_plugins_event_feed() {
    let event = EngineEvent::ExtensionStateCommitted {
        meta: EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: SessionId("fixture-session".to_owned()),
            sequence_id: SequenceId(4),
            emitted_at: "2026-07-11T00:00:00Z".to_owned(),
            caused_by: None,
        },
        plugin_id: "private-plugin".to_owned(),
        transaction: rw_types::extension_contract::ExtensionStateTransaction {
            expected_revision: None,
            mutations: vec![rw_types::extension_contract::ExtensionStateMutation::Set {
                key: "private".into(),
                value: serde_json::json!("not another plugin's capability"),
            }],
            acknowledged: None,
        },
    };
    assert!(rw_types::extension_events::ExtensionEventKind::from_event(&event).is_none());
}
