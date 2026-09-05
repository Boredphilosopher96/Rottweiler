#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;

use rw_types::{Block, ClientCommand, EngineEvent, ToolOutput, Turn};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct ContractFixture {
    turns: Vec<Turn>,
    client_commands: Vec<ClientCommand>,
    engine_events: Vec<EngineEvent>,
}

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/fixtures/contract.json")
}

#[test]
fn generated_fixture_round_trips_through_rust_types() {
    let fixture_text =
        fs::read_to_string(fixture_path()).expect("generated protocol fixture should be present");
    let fixture: ContractFixture =
        serde_json::from_str(&fixture_text).expect("generated fixture must match Rust types");

    let encoded = serde_json::to_value(&fixture.engine_events)
        .expect("typed events should serialize to JSON");
    let decoded: Vec<EngineEvent> =
        serde_json::from_value(encoded).expect("serialized events should deserialize");
    assert_eq!(decoded, fixture.engine_events);

    let encoded = serde_json::to_value(&fixture.client_commands)
        .expect("typed commands should serialize to JSON");
    let decoded: Vec<ClientCommand> =
        serde_json::from_value(encoded).expect("serialized commands should deserialize");
    assert_eq!(decoded, fixture.client_commands);

    let encoded = serde_json::to_value(&fixture.turns).expect("typed turns should serialize");
    let decoded: Vec<Turn> =
        serde_json::from_value(encoded).expect("serialized turns should deserialize");
    assert_eq!(decoded, fixture.turns);
}

#[test]
fn fixture_exercises_every_ir_variant_shape() {
    let fixture_text =
        fs::read_to_string(fixture_path()).expect("generated protocol fixture should be present");
    let fixture: ContractFixture =
        serde_json::from_str(&fixture_text).expect("generated fixture must match Rust types");
    let blocks = &fixture.turns[0].blocks;

    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, Block::Text { .. }))
    );
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, Block::Thinking { .. }))
    );
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, Block::ToolCall { .. }))
    );
    assert!(blocks.iter().any(|block| {
        matches!(
            block,
            Block::ToolResult {
                output: ToolOutput::Mixed { .. },
                ..
            }
        )
    }));
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, Block::Image { .. }))
    );
    assert!(
        blocks
            .iter()
            .any(|block| matches!(block, Block::Citation { .. }))
    );
}

#[test]
fn additive_event_fields_are_tolerated() {
    let fixture_text =
        fs::read_to_string(fixture_path()).expect("generated protocol fixture should be present");
    let fixture_json: Value =
        serde_json::from_str(&fixture_text).expect("fixture should be valid JSON");
    let mut event = fixture_json["engine_events"][0].clone();
    event
        .as_object_mut()
        .expect("fixture event should be an object")
        .insert("future_additive_field".to_owned(), Value::Bool(true));

    let decoded = serde_json::from_value::<EngineEvent>(event);
    assert!(decoded.is_ok());
}

#[test]
fn generated_operation_values_reject_unknown_fields_at_their_owner() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/schema/engine-event.schema.json");
    let schema: Value =
        serde_json::from_slice(&fs::read(path).expect("generated schema")).expect("schema JSON");
    for name in ["ToolProgress", "ProgressAmount"] {
        assert_eq!(
            schema["$defs"][name]["additionalProperties"],
            Value::Bool(false)
        );
    }
    assert!(
        serde_json::from_value::<rw_types::ToolProgress>(serde_json::json!({
            "message": "working", "surprise": true,
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<rw_types::ProgressAmount>(serde_json::json!({
            "completed": 1, "total": 2, "surprise": true,
        }))
        .is_err()
    );
}

#[test]
fn omitted_optional_fields_are_tolerated() {
    let fixture_text =
        fs::read_to_string(fixture_path()).expect("generated protocol fixture should be present");
    let fixture_json: Value =
        serde_json::from_str(&fixture_text).expect("fixture should be valid JSON");

    let mut thinking = fixture_json["turns"][0]["blocks"][1].clone();
    thinking
        .as_object_mut()
        .expect("thinking block should be an object")
        .remove("signature");
    assert!(serde_json::from_value::<Block>(thinking).is_ok());

    let mut compact = fixture_json["client_commands"]
        .as_array()
        .expect("commands should be an array")
        .iter()
        .find(|command| command["type"] == "compact")
        .expect("fixture should contain compact command")
        .clone();
    compact
        .as_object_mut()
        .expect("compact command should be an object")
        .remove("instructions");
    assert!(serde_json::from_value::<ClientCommand>(compact).is_ok());
}

#[test]
fn session_sequence_ids_are_unique_monotonic_decimal_strings() {
    let fixture_text =
        fs::read_to_string(fixture_path()).expect("generated protocol fixture should be present");
    let fixture_json: Value =
        serde_json::from_str(&fixture_text).expect("fixture should be valid JSON");
    let sequence_ids = fixture_json["engine_events"]
        .as_array()
        .expect("events should be an array")
        .iter()
        .filter_map(|event| event["meta"]["sequence_id"].as_str())
        .map(|sequence| {
            sequence
                .parse::<u64>()
                .expect("sequence id should be a decimal u64")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        sequence_ids,
        (0..sequence_ids.len() as u64).collect::<Vec<_>>()
    );
}
