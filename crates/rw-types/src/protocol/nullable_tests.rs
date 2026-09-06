//! Nullable wire values must remain explicit across Rust and generated schemas.
use super::{
    CacheBreakpoint, CostSnapshot, McpApprovalReview, ModelCapabilities, ModelSwitchQuestion, Usage,
};
use crate::transcript::{
    TranscriptAnchor, TranscriptContent, TranscriptContentPage, TranscriptItem, TranscriptRead,
    TranscriptReadResult, TranscriptSubagentStatus, TranscriptView,
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn required_nullable<T: DeserializeOwned + Serialize + JsonSchema>(
    value: Value,
    fields: &[&str],
) -> TestResult {
    let decoded: T = serde_json::from_value(value.clone())?;
    let encoded = serde_json::to_value(decoded)?;
    let schema = serde_json::to_value(schemars::schema_for!(T))?;
    let object = if schema.get("oneOf").is_some() {
        schema["oneOf"]
            .as_array()
            .ok_or("variants")?
            .iter()
            .find(|variant| variant["properties"]["type"]["const"] == value["type"])
            .ok_or("matching variant")?
    } else {
        &schema
    };
    for field in fields {
        assert!(value[*field].is_null(), "explicit null fixture: {field}");
        assert!(encoded.get(*field).is_some_and(Value::is_null));
        assert!(
            object["required"]
                .as_array()
                .ok_or("required fields")?
                .contains(&json!(field)),
            "schema must require {field}"
        );
        assert!(object["properties"][*field].get("default").is_none());
        let mut missing = value.clone();
        missing
            .as_object_mut()
            .ok_or("fixture object")?
            .remove(*field);
        assert!(
            serde_json::from_value::<T>(missing).is_err(),
            "accepted absent {field}"
        );
    }
    Ok(())
}

fn view() -> Value {
    json!({"session_id":"fixture","projection_version":1,"generation":"0",
        "through":null,"digest":vec![0;32]})
}
fn source() -> Value {
    json!({"sequence":"1","selector":{"type":"conversation"}})
}
fn preview() -> Value {
    json!({"text":"body","format":"text","complete":true,"source":source()})
}

#[test]
fn transcript_nullable_cursors_and_identities_reject_missing_keys() -> TestResult {
    required_nullable::<TranscriptView>(view(), &["through"])?;
    required_nullable::<TranscriptRead>(
        json!({"known_view":null,"position":{"type":"latest"},
        "max_items":1,"max_bytes":4096}),
        &["known_view"],
    )?;
    required_nullable::<TranscriptItem>(
        json!({"id":"1","ordinal":"0","revision":"1",
        "agent_turn":null,"content":{"type":"conversation","role":"user","blocks":[],
        "omitted_blocks":false,"source":source()}}),
        &["agent_turn"],
    )?;
    required_nullable::<TranscriptContentPage>(
        json!({"view":view(),"source":source(),
        "offset":0,"next_offset":null,"total_bytes":0,"format":"text","text":""}),
        &["next_offset"],
    )?;
    required_nullable::<TranscriptAnchor>(
        json!({"type":"replaced","requested":"1",
        "replacement":null}),
        &["replacement"],
    )?;
    required_nullable::<TranscriptReadResult>(
        json!({"type":"catching_up","through":null,
        "target":null}),
        &["through", "target"],
    )?;
    Ok(())
}

#[test]
fn transcript_nullable_display_fields_reject_missing_keys() -> TestResult {
    required_nullable::<TranscriptContent>(
        json!({"type":"tool","invocation_id":"host-call",
        "name":"read","call_index":0,"arguments":preview(),"diff":null,
        "status":{"type":"running"}}),
        &["diff"],
    )?;
    required_nullable::<TranscriptContent>(
        json!({"type":"shell","command":null,
        "output":null,"active":false,"status":null}),
        &["command", "output", "status"],
    )?;
    required_nullable::<TranscriptSubagentStatus>(
        json!({"type":"finished","status":"completed",
        "result":preview(),"touched_file_count":0,"diff":null}),
        &["diff"],
    )?;
    Ok(())
}

#[test]
fn model_and_approval_nullability_does_not_accept_omitted_fields() -> TestResult {
    required_nullable::<ModelSwitchQuestion>(
        json!({"model":"primary","provider":null}),
        &["provider"],
    )?;
    required_nullable::<McpApprovalReview>(
        json!({"server":"fixture","transport":"stdio",
        "endpoint":null,"origin":"user","defer_tools":false,"fingerprint":"fixture",
        "previously_approved":false}),
        &["endpoint"],
    )?;
    let mut capabilities = json!({"tool_calling":true,"vision":false,"thinking":false,
        "cache_behavior":"none","max_context_tokens":null,"max_output_tokens":null});
    required_nullable::<ModelCapabilities>(
        capabilities.clone(),
        &["max_context_tokens", "max_output_tokens"],
    )?;
    capabilities["max_context_tokens"] = json!("18446744073709551615");
    capabilities["max_output_tokens"] = json!("4096");
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<ModelCapabilities>(
            capabilities.clone()
        )?)?,
        capabilities
    );
    Ok(())
}

const COST_LIMITS: &[&str] = &[
    "session_cost_cap_micros_usd",
    "daily_cost_cap_micros_usd",
    "session_ai_credit_cap_micros",
    "daily_ai_credit_cap_micros",
    "session_token_cap",
    "daily_token_cap",
    "spend_rate_alarm_micros_usd_per_minute",
    "ai_credit_rate_alarm_micros_per_minute",
    "token_rate_alarm_per_minute",
];

#[test]
fn every_cost_limit_requires_an_explicit_nullable_decimal() -> TestResult {
    let mut value = json!({"utc_day":"2026-09-06","subscription_quota":null,
        "session_usage":Usage { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0 },"cache_hit_basis_points":0,"hard_cap_reached":false,
        "session_monetary_accounting_complete":true,"daily_monetary_accounting_complete":true});
    for field in [
        "session_cost_micros_usd",
        "session_ai_credit_micros",
        "daily_cost_micros_usd",
        "daily_ai_credit_micros",
        "trailing_minute_cost_micros_usd",
        "trailing_minute_ai_credit_micros",
        "session_subscription_tokens",
        "daily_subscription_tokens",
        "trailing_minute_subscription_tokens",
        "session_subscription_quota_entries",
        "session_cost_unavailable_entries",
        "session_non_usd_monetary_entries",
        "daily_subscription_quota_entries",
        "daily_cost_unavailable_entries",
        "daily_non_usd_monetary_entries",
    ] {
        value[field] = json!("0");
    }
    for field in COST_LIMITS {
        value[*field] = Value::Null;
    }
    required_nullable::<CostSnapshot>(value.clone(), COST_LIMITS)?;
    for field in COST_LIMITS {
        value[*field] = json!("18446744073709551615");
    }
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<CostSnapshot>(value.clone())?)?,
        value
    );
    Ok(())
}

#[test]
fn empty_cache_boundary_requires_its_explicit_source_field() -> TestResult {
    required_nullable::<CacheBreakpoint>(json!({"after_item_id":null}), &["after_item_id"])
}
