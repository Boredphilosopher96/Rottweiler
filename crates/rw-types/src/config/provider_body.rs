//! One request-field authority for config parsing and direct adapter construction.
/// Whether an extra body field would override engine-owned request semantics.
/// Responses text verbosity is independent; its format is an output contract.
#[must_use]
pub fn provider_body_field_is_controlled(key: &str, value: &serde_json::Value) -> bool {
    let lower = key.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "model"
            | "messages"
            | "input"
            | "tools"
            | "tool_choice"
            | "stream"
            | "stream_options"
            | "max_tokens"
            | "max_completion_tokens"
            | "max_output_tokens"
            | "temperature"
            | "response_format"
            | "reasoning"
    ) || lower.starts_with("reasoning_")
        || (lower == "text"
            && value
                .as_object()
                .is_some_and(|fields| fields.keys().any(|key| key.eq_ignore_ascii_case("format"))))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_selection_cannot_escape_the_typed_output_contract() {
        for (key, value) in [
            ("response_format", serde_json::json!({"type":"json_object"})),
            ("text", serde_json::json!({"format":{"type":"json_schema"}})),
            ("tools", serde_json::json!([])),
        ] {
            assert!(provider_body_field_is_controlled(key, &value));
            let configuration = super::super::ProviderConfig {
                kind: "openai_compatible".into(),
                extra_body: std::collections::BTreeMap::from([(key.into(), value)]),
                ..super::super::ProviderConfig::default()
            };
            assert!(configuration.validate_gateway_options().is_err());
        }
        assert!(!provider_body_field_is_controlled(
            "text",
            &serde_json::json!({"verbosity":"low"})
        ));
        assert!(!provider_body_field_is_controlled(
            "service_tier",
            &serde_json::json!("default")
        ));
    }
}
