use rw_providers::{ProviderEvent, ProviderRequest};
use serde_json::{Value, json};

fn request() -> Value {
    json!({
        "model": "fixture", "turns": [], "tools": [], "tool_choice": {"mode": "auto"},
        "max_output_tokens": 64, "temperature": null, "thinking": "off", "cache_hint": null
    })
}

#[test]
fn provider_request_requires_its_complete_envelope() -> Result<(), Box<dyn std::error::Error>> {
    let complete = request();
    assert!(serde_json::from_value::<ProviderRequest>(complete.clone()).is_ok());
    for field in [
        "model",
        "turns",
        "tools",
        "tool_choice",
        "max_output_tokens",
        "temperature",
        "thinking",
        "cache_hint",
    ] {
        let mut incomplete = complete.clone();
        incomplete
            .as_object_mut()
            .ok_or("request object")?
            .remove(field);
        assert!(
            serde_json::from_value::<ProviderRequest>(incomplete).is_err(),
            "accepted missing {field}"
        );
    }
    Ok(())
}

#[test]
fn provider_boundaries_reject_foreign_fields() {
    assert!(
        serde_json::from_value::<ProviderEvent>(json!({"type":"route_selected","route":"forged"}))
            .is_err()
    );
    let mut extra = request();
    extra["extra"] = json!(true);
    assert!(serde_json::from_value::<ProviderRequest>(extra).is_err());
    let mut tool_choice = request();
    tool_choice["tool_choice"]["name"] = json!("unexpected");
    assert!(serde_json::from_value::<ProviderRequest>(tool_choice).is_err());
    assert!(
        serde_json::from_value::<ProviderEvent>(
            json!({"type":"text_delta","text":"hello","arguments":{}})
        )
        .is_err()
    );
}

#[test]
fn nullable_event_fields_are_explicit_and_conversation_metadata_is_optional() {
    assert!(
        serde_json::from_value::<ProviderEvent>(
            json!({"type":"thinking_delta","content":"reason"})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<ProviderEvent>(
            json!({"type":"thinking_delta","content":"reason","signature":null})
        )
        .is_ok()
    );
    let mut with_history = request();
    with_history["turns"] = json!([{
        "role": "assistant", "meta": {"synthetic":false,"summary":false},
        "blocks": [{"type":"thinking","content":"reason"},
                   {"type":"citation","uri":"https://example.com"}]
    }]);
    assert!(serde_json::from_value::<ProviderRequest>(with_history).is_ok());
}
