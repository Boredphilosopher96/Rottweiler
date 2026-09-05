use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{ClientCommand, CommandOutcome, EngineEvent, McpServerState};
use crate::transcript::{
    TranscriptAnchor, TranscriptContentSelector, TranscriptInvalidation, TranscriptPosition,
    TranscriptSubagentStatus, TranscriptToolStatus,
};
use crate::{Block, ImageRef, ToolOutput, ToolOutputPart, Turn};

fn reject_extra<T: DeserializeOwned>(value: Value) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::from_value::<T>(value.clone())?;
    let mut invalid = value;
    invalid
        .as_object_mut()
        .ok_or("expected object")?
        .insert("undeclared".to_owned(), json!(true));
    assert!(serde_json::from_value::<T>(invalid).is_err());
    Ok(())
}

#[test]
fn engine_contract_rejects_undeclared_envelope_and_ir_fields()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../../protocol/fixtures/contract.json"))?;
    for value in fixture["engine_events"]
        .as_array()
        .ok_or("missing events")?
    {
        reject_extra::<EngineEvent>(value.clone())?;
    }
    for value in fixture["client_commands"]
        .as_array()
        .ok_or("missing commands")?
    {
        reject_extra::<ClientCommand>(value.clone())?;
    }
    for value in fixture["turns"].as_array().ok_or("missing turns")? {
        reject_extra::<Turn>(value.clone())?;
    }
    reject_extra::<ToolOutput>(json!({"type":"text", "text":"hello"}))?;
    reject_extra::<ToolOutputPart>(json!({"type":"structured", "value":{"arbitrary":true}}))?;
    reject_extra::<ImageRef>(json!({"type":"url", "url":"https://example.test/image"}))?;
    reject_extra::<Block>(json!({"type":"text", "text":"hello"}))?;
    // Values explicitly declared as arbitrary JSON remain open within their typed envelope.
    serde_json::from_value::<ToolOutput>(
        json!({"type":"structured", "value":{"arbitrary":{"nested":true}}}),
    )?;
    Ok(())
}

#[test]
fn zero_payload_tagged_variants_reject_extra_fields() -> Result<(), Box<dyn std::error::Error>> {
    fn inspect<T: DeserializeOwned + JsonSchema>() -> Result<(), Box<dyn std::error::Error>> {
        let schema = serde_json::to_value(schemars::schema_for!(T))?;
        for variant in schema["oneOf"].as_array().ok_or("missing variants")? {
            let properties = variant["properties"]
                .as_object()
                .ok_or("missing properties")?;
            if properties.len() != 1 {
                continue;
            }
            let (name, tag) = properties.iter().next().ok_or("missing tag")?;
            let constant = tag.get("const").ok_or("missing constant tag")?;
            reject_extra::<T>(json!({ name: constant }))?;
        }
        Ok(())
    }
    inspect::<CommandOutcome>()?;
    inspect::<McpServerState>()?;
    inspect::<TranscriptContentSelector>()?;
    inspect::<TranscriptToolStatus>()?;
    inspect::<TranscriptSubagentStatus>()?;
    inspect::<TranscriptPosition>()?;
    inspect::<TranscriptInvalidation>()?;
    inspect::<TranscriptAnchor>()?;
    Ok(())
}

#[test]
fn wire_object_schemas_explicitly_own_their_field_sets() -> Result<(), Box<dyn std::error::Error>> {
    fn inspect(value: &Value, path: &str) {
        match value {
            Value::Object(object) => {
                if object.get("type").and_then(Value::as_str) == Some("object") {
                    assert!(
                        object.contains_key("additionalProperties"),
                        "open object schema {path}"
                    );
                }
                for (key, child) in object {
                    inspect(child, &format!("{path}/{key}"));
                }
            }
            Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    inspect(child, &format!("{path}/{index}"));
                }
            }
            _ => {}
        }
    }
    for schema in [
        serde_json::to_value(schemars::schema_for!(EngineEvent))?,
        serde_json::to_value(schemars::schema_for!(ClientCommand))?,
        serde_json::to_value(schemars::schema_for!(super::CommandReply))?,
    ] {
        inspect(&schema, "");
    }
    Ok(())
}
