#![allow(clippy::expect_used)]

use super::*;
use serde_json::json;

#[test]
fn transaction_has_explicit_revision_and_acknowledgement_fields() {
    let valid = json!({
        "expected_revision": null,
        "mutations": [{"action":"set", "key":"tasks/1", "value":{"done":false}}],
        "acknowledged": null,
    });
    let parsed: ExtensionStateTransaction =
        serde_json::from_value(valid.clone()).expect("transaction");
    validate_state_transaction(&parsed).expect("valid state change");
    for field in ["expected_revision", "acknowledged"] {
        let mut missing = valid.clone();
        missing.as_object_mut().expect("object").remove(field);
        assert!(serde_json::from_value::<ExtensionStateTransaction>(missing).is_err());
    }
    let mut extra = valid;
    extra["plugin_id"] = json!("another-plugin");
    assert!(serde_json::from_value::<ExtensionStateTransaction>(extra).is_err());
}

#[test]
fn ambiguous_and_oversized_state_changes_are_rejected() {
    let mutation = ExtensionStateMutation::Delete {
        key: "tasks/1".to_owned(),
    };
    let mut transaction = ExtensionStateTransaction {
        expected_revision: None,
        mutations: vec![mutation.clone(), mutation],
        acknowledged: None,
    };
    assert_eq!(
        validate_state_transaction(&transaction),
        Err(ExtensionStateError("transaction contains a duplicate key"))
    );
    transaction.mutations.clear();
    assert!(validate_state_transaction(&transaction).is_err());
    transaction.mutations.push(ExtensionStateMutation::Set {
        key: "tasks/1".to_owned(),
        value: json!("x".repeat(MAX_EXTENSION_STATE_VALUE_BYTES)),
    });
    assert!(validate_state_transaction(&transaction).is_err());
    for key in ["", "/task", "task\n", "é", "task key"] {
        assert!(validate_state_key(key).is_err());
    }
}

#[test]
fn value_budget_counts_json_escaping_and_multibyte_text() {
    for value in [json!("é\n\""), json!({"items":[1, true, null]})] {
        assert_eq!(
            state_value_bytes(&value).expect("bounded"),
            serde_json::to_vec(&value).expect("JSON").len()
        );
    }
    let boundary = json!("a".repeat(MAX_EXTENSION_STATE_VALUE_BYTES - 2));
    assert_eq!(
        state_value_bytes(&boundary).expect("exact bound"),
        MAX_EXTENSION_STATE_VALUE_BYTES
    );
}
