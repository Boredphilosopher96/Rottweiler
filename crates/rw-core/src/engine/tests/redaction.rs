#![cfg(test)]

use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::redaction::NoopSecretRedactor;
use crate::engine::redaction::StreamingSecretRedactor;
use crate::engine::tests::fixtures::hooks::FixedHook;
use crate::engine::tests::fixtures::hooks::PayloadCaptureHook;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::CanarySecretRedactor;
use crate::engine::tests::fixtures::support::PemSecretRedactor;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use crate::engine::turn::redacted_json;
use rw_ext::HookDirective;
use rw_ext::HookError;
use rw_ext::HookEvent;
use rw_ext::HookFailurePolicy;
use rw_ext::HookRegistration;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::ApprovalDecision;
use rw_types::ToolCapability;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::TempDir;

#[test]
fn streaming_redaction_holds_split_secrets_until_they_are_safe() {
    let redactor = CanarySecretRedactor;
    let mut stream = StreamingSecretRedactor::new(&redactor);
    let mut visible = stream.push("prefix KNOWN_");
    assert!(!visible.contains("KNOWN_"));
    visible.push_str(&stream.push("CANARY suffix"));
    visible.push_str(&stream.finish());
    assert_eq!(visible, "prefix [REDACTED] suffix");
    assert!(!visible.contains("KNOWN_CANARY"));
}

#[test]
fn streaming_redaction_never_exposes_an_unterminated_private_key() {
    let redactor = PemSecretRedactor;
    let mut stream = StreamingSecretRedactor::new(&redactor);
    let mut visible = stream.push(
            "safe\n-----BEGIN PRIVATE KEY-----\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        );
    assert!(visible.is_empty());
    visible.push_str(&stream.push(
            "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\n-----END PRIVATE KEY-----\nafter",
        ));
    visible.push_str(&stream.finish());
    assert_eq!(visible, "safe\n[REDACTED]\nafter");
    assert!(!visible.contains("AAAA"));
    assert!(!visible.contains("BBBB"));
}

#[test]
fn streaming_redaction_drops_a_private_key_when_the_stream_ends_unterminated() {
    let redactor = PemSecretRedactor;
    let mut stream = StreamingSecretRedactor::new(&redactor);
    let mut visible = stream.push(
            "safe\n-----BEGIN PRIVATE KEY-----\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        );
    visible.push_str(&stream.finish());
    assert_eq!(visible, "[REDACTED]");
    assert!(!visible.contains("AAAA"));
    assert!(!visible.contains("PRIVATE KEY"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn secrets_never_reach_durable_tool_events_or_hook_payloads() {
    let root = TempDir::new().expect("tempdir");
    let raw_arguments = json!({
        "api_key": "KEY_CANARY",
        "known_value": "KNOWN_CANARY",
        "nested": {"password": "PASS_CANARY"},
        "safe": "visible",
    });
    let model = Arc::new(ScriptedModel::new([
        tool_script(&[("call", "fixture", raw_arguments.clone())], &[]),
        stop_script("done", &[]),
    ]));
    let tool = Arc::new(StubTool::new(
        "fixture",
        vec![ToolCapability::WriteFilesystem],
        StubOutcome::Success(ToolResult::new(
            "KNOWN_CANARY output",
            json!({
                "authorization": "Bearer OUTPUT_CANARY",
                "safe": "visible output",
            }),
        )),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register tool");
    let payloads = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    for (id, event, label) in [
        (
            "fixture.capture-permission",
            HookEvent::PermissionCheck,
            "permission_check",
        ),
        ("fixture.capture-pre", HookEvent::PreTool, "pre_tool"),
        ("fixture.capture-post", HookEvent::PostTool, "post_tool"),
    ] {
        hooks
            .register(
                HookRegistration::new(id, event, rw_types::hook_contract::HookClass::Policy),
                PayloadCaptureHook {
                    label,
                    payloads: payloads.clone(),
                },
            )
            .expect("capture hook");
    }
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        hooks,
    );
    actor_config.event_sink = sink.clone();
    actor_config.secret_redactor = Arc::new(CanarySecretRedactor);
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    assert_eq!(request.arguments["safe"], "visible");
    assert_eq!(request.arguments["api_key"], "[REDACTED]");
    assert_eq!(request.arguments["known_value"], "[REDACTED]");
    assert_eq!(request.arguments["nested"]["password"], "[REDACTED]");
    assert!(
        handle
            .approve(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::AllowOnce
            )
            .await
            .expect("approval")
    );
    collect_turn(&mut events).await;

    assert_eq!(
        tool.inputs.lock().expect("tool inputs").as_slice(),
        &[raw_arguments],
        "the tool execution boundary still receives the original arguments"
    );
    let captured = payloads.lock().expect("captured hook payloads").clone();
    assert_eq!(
        captured.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
        ["permission_check", "pre_tool", "post_tool"]
    );
    let hook_wire = serde_json::to_string(&captured).expect("serialize hook payloads");
    let durable_wire = serde_json::to_string(
        &sink
            .events
            .lock()
            .expect("durable events")
            .iter()
            .map(|event| &event.wire)
            .collect::<Vec<_>>(),
    )
    .expect("serialize durable events");
    for exposed in ["KEY_CANARY", "KNOWN_CANARY", "PASS_CANARY", "OUTPUT_CANARY"] {
        assert!(!hook_wire.contains(exposed), "hook exposed {exposed}");
        assert!(!durable_wire.contains(exposed), "event exposed {exposed}");
    }
    assert!(hook_wire.contains("visible"));
    assert!(hook_wire.contains("visible output"));
    assert!(hook_wire.contains("[REDACTED]"));
    assert!(durable_wire.contains("visible"));
    assert!(durable_wire.contains("visible output"));
    assert!(durable_wire.contains("[REDACTED]"));
}

#[tokio::test]
async fn hook_failure_and_block_messages_are_redacted_before_events() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(&[("call", "fixture", json!({}))], &[]),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "fixture",
            vec![ToolCapability::WriteFilesystem],
            StubOutcome::Success(ToolResult::new("unused", Value::Null)),
        )))
        .expect("register tool");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new(
                "fixture.secret-failure",
                HookEvent::PermissionCheck,
                rw_types::hook_contract::HookClass::Observer,
            )
            .with_failure_policy(HookFailurePolicy::FailOpen),
            FixedHook {
                label: "failure",
                calls: calls.clone(),
                result: Err(HookError::new("fixture", "KNOWN_CANARY failure")),
            },
        )
        .expect("failure hook");
    hooks
        .register(
            HookRegistration::new(
                "fixture.secret-block",
                HookEvent::PreTool,
                rw_types::hook_contract::HookClass::Policy,
            ),
            FixedHook {
                label: "block",
                calls,
                result: Ok(HookDirective::Block {
                    message: "KNOWN_CANARY blocked".to_owned(),
                }),
            },
        )
        .expect("blocking hook");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        hooks,
    );
    actor_config.event_sink = sink.clone();
    actor_config.secret_redactor = Arc::new(CanarySecretRedactor);
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    assert!(
        handle
            .approve(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::AllowOnce
            )
            .await
            .expect("approval")
    );
    collect_turn(&mut events).await;
    let durable = serde_json::to_string(
        &sink
            .events
            .lock()
            .expect("durable events")
            .iter()
            .map(|event| &event.wire)
            .collect::<Vec<_>>(),
    )
    .expect("serialize durable events");
    assert!(!durable.contains("KNOWN_CANARY"));
    assert!(durable.contains("[REDACTED]"));
    assert!(durable.contains("blocked"));
}

#[tokio::test]
async fn user_secrets_are_redacted_before_hooks_events_and_provider_context() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
    let payloads = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new(
                "fixture.capture-user",
                HookEvent::UserPromptSubmit,
                rw_types::hook_contract::HookClass::Observer,
            ),
            PayloadCaptureHook {
                label: "user_prompt_submit",
                payloads: payloads.clone(),
            },
        )
        .expect("capture hook");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        hooks,
    );
    actor_config.event_sink = sink.clone();
    actor_config.secret_redactor = Arc::new(CanarySecretRedactor);
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("safe KNOWN_CANARY tail")
        .await
        .expect("message");
    collect_turn(&mut events).await;

    let hook_wire = serde_json::to_string(&*payloads.lock().expect("hook payloads"))
        .expect("serialize hook payloads");
    let durable_wire = serde_json::to_string(
        &sink
            .events
            .lock()
            .expect("durable events")
            .iter()
            .map(|event| &event.wire)
            .collect::<Vec<_>>(),
    )
    .expect("serialize durable events");
    let provider_wire = serde_json::to_string(&*model.requests.lock().expect("requests"))
        .expect("serialize provider requests");
    for wire in [&hook_wire, &durable_wire, &provider_wire] {
        assert!(!wire.contains("KNOWN_CANARY"));
        assert!(wire.contains("[REDACTED]"));
        assert!(wire.contains("safe"));
    }
}

#[test]
fn structured_token_metrics_are_not_mistaken_for_credentials() {
    let value = redacted_json(
        json!({
            "max_tokens": 4096,
            "input_tokens": 12,
            "output_tokens": 34,
            "token_count": 46,
            "token_type": "cached",
            "access_token": "credential",
        }),
        &NoopSecretRedactor,
    );
    assert_eq!(value["max_tokens"], 4096);
    assert_eq!(value["input_tokens"], 12);
    assert_eq!(value["output_tokens"], 34);
    assert_eq!(value["token_count"], 46);
    assert_eq!(value["token_type"], "cached");
    assert_eq!(value["access_token"], "[REDACTED]");
}
