#![allow(clippy::expect_used)]
use super::*;
use serde_json::json;

fn definition(server: &str, name: &str, schema: Value) -> McpToolDefinition {
    McpToolDefinition {
        server: rw_types::McpServerId::new(server).expect("server"),
        name: name.into(),
        description: format!("definition {name}"),
        input_schema: schema,
        capabilities: McpToolDefinition::restrictive_capabilities(),
    }
}

#[test]
fn selected_output_keeps_exact_schema_and_filters_before_computing_truncation() {
    let schema = json!({"type":"object","properties":{"text":{"const":"λ\n\0quoted\""}}});
    let source = vec![
        definition(
            "foreign",
            "hidden",
            json!({"large":"x".repeat(MAX_SCHEMAS_BYTES)}),
        ),
        definition("allowed", "exact", schema.clone()),
    ];
    let policy = McpToolPolicy::restricted(["mcp:allowed/exact".into()]).expect("policy");
    let encoded = encode_selected(&source, &policy).expect("selected output");
    let output: Value = serde_json::from_slice(&encoded).expect("JSON");
    assert_eq!(output["truncated"], false);
    assert_eq!(output["matches"].as_array().expect("matches").len(), 1);
    assert_eq!(output["matches"][0]["input_schema"], schema);
}

#[test]
fn output_retains_a_whole_prefix_and_never_partially_truncates_a_schema() {
    let source = vec![
        definition("server", "first", json!({"first":true})),
        definition(
            "server",
            "large",
            json!({"text":"x".repeat(MAX_SCHEMAS_BYTES)}),
        ),
        definition("server", "after", json!({"after":true})),
    ];
    let encoded = encode_selected(&source, &McpToolPolicy::default()).expect("bounded output");
    assert!(encoded.len() <= MAX_OUTPUT_BYTES);
    let output: Value = serde_json::from_slice(&encoded).expect("JSON");
    assert_eq!(output["truncated"], true);
    assert_eq!(output["matches"].as_array().expect("matches").len(), 1);
    assert_eq!(
        output["matches"][0],
        serde_json::to_value(&source[0]).expect("exact definition")
    );
}

#[test]
fn per_definition_counts_equal_the_complete_array_encoding_at_the_byte_boundary() {
    let mut source = vec![definition("server", "boundary", json!({"text":""}))];
    let overhead = serde_json::to_vec(&source).expect("array overhead").len();
    source[0].input_schema["text"] = Value::String("x".repeat(MAX_SCHEMAS_BYTES - overhead));
    assert_eq!(
        serde_json::to_vec(&source).expect("exact array").len(),
        MAX_SCHEMAS_BYTES
    );
    let output: Value = serde_json::from_slice(
        &encode_selected(&source, &McpToolPolicy::default()).expect("exact fit"),
    )
    .expect("JSON");
    assert_eq!(output["truncated"], false);
    source[0].input_schema["text"] = Value::String("x".repeat(MAX_SCHEMAS_BYTES - overhead + 1));
    let output: Value = serde_json::from_slice(
        &encode_selected(&source, &McpToolPolicy::default()).expect("oversized first schema"),
    )
    .expect("JSON");
    assert_eq!(output["truncated"], true);
    assert_eq!(output["matches"], json!([]));
}
