#![allow(clippy::expect_used)]
use super::*;
use rw_types::ToolInvocationId;

fn execution(text: String) -> ToolExecution {
    ToolExecution {
        presentation: None,
        unsettled: false,
        call: PendingToolCall {
            id: "provider-alias".into(),
            invocation_id: ToolInvocationId("host-invocation".into()),
            name: "fixture".into(),
            arguments: Some(serde_json::json!({"input":"é\\\"\n"})),
            index: 0,
        },
        output: ToolOutput::Text { text },
        is_error: true,
    }
}

#[test]
fn digest_preserves_failure_fields_and_ignores_invocation_aliases() {
    let mut result = execution("failure\né".into());
    let initial = signature(&result.call, &result.output).expect("digest");
    result.call.id = "reused-provider-alias".into();
    result.call.invocation_id = ToolInvocationId("another-invocation".into());
    assert_eq!(signature(&result.call, &result.output), Some(initial));
    result.call.name.push('x');
    assert_ne!(signature(&result.call, &result.output), Some(initial));
    result.call.name.pop();
    result.call.arguments = None;
    assert_ne!(signature(&result.call, &result.output), Some(initial));
}

#[test]
fn distinct_large_failures_retain_only_fixed_digest_slots() {
    let mut guard = DoomLoopGuard::new(3);
    for index in 0..100 {
        let result = execution(format!("{index}:{}", "x".repeat(256 * 1024)));
        assert!(!guard.observe(&result.call, &result));
        assert!(guard.recent_failures.len() <= 12);
        assert!(guard.recent_failures.capacity() <= 32);
    }
    assert_eq!(std::mem::size_of::<blake3::Hash>(), 32);
    assert!(guard.recent_failures.iter().all(Option::is_some));
}

#[test]
fn repeated_failures_and_successful_calls_preserve_window_decay() {
    let mut guard = DoomLoopGuard::new(2);
    let mut result = execution("failed".into());
    assert!(!guard.observe(&result.call, &result));
    assert!(guard.observe(&result.call, &result));
    result.is_error = false;
    for _ in 0..8 {
        assert!(!guard.observe(&result.call, &result));
    }
    result.is_error = true;
    assert!(!guard.observe(&result.call, &result));
    assert!(guard.observe(&result.call, &result));
}

#[test]
fn refused_encoding_does_not_collapse_different_failures_into_one_identity() {
    let mut guard = DoomLoopGuard::new(2);
    let result = execution("x".repeat(MAX_SIGNATURE_BYTES));
    assert_eq!(signature(&result.call, &result.output), None);
    for _ in 0..3 {
        assert!(!guard.observe(&result.call, &result));
    }
}
