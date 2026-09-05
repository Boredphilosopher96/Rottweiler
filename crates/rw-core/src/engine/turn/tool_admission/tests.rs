#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{AdmittedToolBatch, PendingToolBudget};
use rw_types::tool_admission::{
    MAX_PENDING_TOOL_ARGUMENT_BYTES, MAX_PENDING_TOOL_INVOCATIONS, MAX_PENDING_TOOL_PREPARED_BYTES,
};
use serde_json::Value;

#[test]
fn pending_count_and_retained_metadata_are_admitted_before_announcement() {
    let mut budget = PendingToolBudget::default();
    for index in 0..MAX_PENDING_TOOL_INVOCATIONS {
        budget
            .start(&format!("call-{index}"), &"read".to_owned())
            .expect("admitted");
    }
    assert!(budget.start(&"overflow".into(), &"read".into()).is_err());
    let mut id = String::with_capacity(MAX_PENDING_TOOL_PREPARED_BYTES);
    id.push('a');
    assert!(
        PendingToolBudget::default()
            .start(&id, &"read".into())
            .is_err()
    );
}

#[test]
fn argument_and_rewrite_admission_is_aggregate_and_failed_update_does_not_release_bytes() {
    let value = Value::String("x".repeat(MAX_PENDING_TOOL_ARGUMENT_BYTES / 2 - 2));
    let mut budget = PendingToolBudget::default();
    budget.arguments(&value).expect("first");
    budget.arguments(&value).expect("second");
    assert!(budget.arguments(&Value::Null).is_err());
    let large = Value::String("x".repeat(MAX_PENDING_TOOL_ARGUMENT_BYTES));
    assert!(budget.replace(Some(&value), &large).is_err());
    assert!(budget.arguments(&Value::Null).is_err());
    budget
        .replace(Some(&value), &Value::Null)
        .expect("smaller rewrite");
    budget
        .arguments(&Value::Null)
        .expect("released exact bytes");
}

#[test]
fn streamed_argument_bytes_are_bounded_before_the_final_json_value() {
    let mut budget = PendingToolBudget::default();
    budget
        .delta(&" ".repeat(MAX_PENDING_TOOL_ARGUMENT_BYTES))
        .expect("limit");
    assert!(budget.delta(" ").is_err());
}

struct ExpandingRedactor;
impl crate::engine::SecretRedactor for ExpandingRedactor {
    fn redact(&self, _text: &str) -> String {
        "x".repeat(MAX_PENDING_TOOL_ARGUMENT_BYTES + 1)
    }
}
#[test]
fn redacted_announcement_must_fit_before_the_batch_can_execute() {
    let call = super::PendingToolCall {
        id: "id".into(),
        invocation_id: rw_types::ToolInvocationId("invocation".into()),
        name: "read".into(),
        arguments: Some(Value::String("secret".into())),
        index: 0,
    };
    assert!(AdmittedToolBatch::new(vec![call], &ExpandingRedactor).is_err());
}
