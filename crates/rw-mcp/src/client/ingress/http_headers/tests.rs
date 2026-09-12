#![allow(clippy::expect_used)]
use super::*;
use base64::{Engine as _, prelude::BASE64_STANDARD};
use rmcp::model::{CallToolRequestParams, CustomRequest, JsonObject, RequestId};
use serde_json::json;

fn schema(properties: &Value) -> JsonObject {
    json!({"type":"object","properties":properties})
        .as_object()
        .expect("schema")
        .clone()
}
fn value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}
fn call(arguments: &Value) -> ClientJsonRpcMessage {
    ClientJsonRpcMessage::request(
        ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
            CallToolRequestParams::new("deploy")
                .with_arguments(arguments.as_object().expect("arguments").clone()),
        )),
        RequestId::Number(1),
    )
}

#[test]
fn annotated_primitives_and_name_use_exact_standard_headers_without_body_copies() {
    let schema = schema(&json!({
        "region":{"type":"string","x-mcp-header":"Region"},
        "count":{"type":"integer","x-mcp-header":"Count"},
        "flag":{"type":"boolean","x-mcp-header":"Flag"},
        "skip":{"type":"string","x-mcp-header":"Skip"}
    }));
    let annotations = ToolHeaderAnnotations::extract(&schema)
        .expect("admission")
        .expect("valid");
    let request = call(
        &json!({"region":"us-west1","count":42,"flag":true,"skip":null,"body":{"secret":"not a header"}}),
    );
    let headers = project(
        &request,
        Some(&annotations),
        &ProtocolVersion::STANDARD_HEADERS,
    )
    .expect("projection");
    assert_eq!(headers.len(), 6);
    assert_eq!(value(&headers, "mcp-method"), Some("tools/call"));
    assert_eq!(value(&headers, "mcp-name"), Some("deploy"));
    assert_eq!(value(&headers, "mcp-param-region"), Some("us-west1"));
    assert_eq!(value(&headers, "mcp-param-count"), Some("42"));
    assert_eq!(value(&headers, "mcp-param-flag"), Some("true"));
    assert!(value(&headers, "mcp-param-skip").is_none());
}

#[test]
fn non_ascii_whitespace_controls_and_sentinel_collisions_roundtrip_through_base64() {
    for original in [
        "λ",
        " padded ",
        "line\r\nInjection: rejected",
        "=?base64?QQ==?=",
        "\t",
    ] {
        let encoded = encoding::encode(original).expect("encoding");
        assert_eq!(
            encoded.len(),
            encoding::encoded_len(original).expect("size")
        );
        let raw = encoded
            .strip_prefix("=?base64?")
            .expect("prefix")
            .strip_suffix("?=")
            .expect("suffix");
        assert_eq!(
            BASE64_STANDARD.decode(raw).expect("base64"),
            original.as_bytes()
        );
        assert!(http::HeaderValue::from_str(&encoded).is_ok());
    }
    for plain in ["", "a b", "ASCII", "42"] {
        assert_eq!(encoding::encode(plain).expect("plain"), plain);
    }
}

#[test]
fn invalid_annotation_rejects_the_tool_without_copying_the_schema() {
    for properties in [
        json!({"a":{"type":"number","x-mcp-header":"A"}}),
        json!({"a":{"type":"array","x-mcp-header":"A"}}),
        json!({"a":{"type":"string","x-mcp-header":""}}),
        json!({"a":{"type":"string","x-mcp-header":"bad:name"}}),
        json!({"a":{"type":"string","x-mcp-header":true}}),
        json!({"a":{"type":"string","x-mcp-header":"Region"},"b":{"type":"integer","x-mcp-header":"region"}}),
        json!({"a":{"type":"object","properties":{"b":{"type":"string","x-mcp-header":"B"}}}}),
    ] {
        assert!(
            ToolHeaderAnnotations::extract(&schema(&properties))
                .expect("no local pressure")
                .is_none()
        );
    }
    let mut original = schema(
        &json!({"region":{"type":"string","x-mcp-header":"Region","description":"x".repeat(1024*1024)}}),
    );
    let retained = ToolHeaderAnnotations::extract(&original)
        .expect("admission")
        .expect("valid");
    original.clear();
    assert_eq!(
        retained.pairs,
        vec![("region".into(), "mcp-param-region".into())]
    );
}

#[test]
fn request_metadata_overrides_negotiated_version_and_unrelated_custom_fields_stay_data() {
    let mut message = call(&json!({}));
    assert_eq!(
        project(&message, None, &ProtocolVersion::V_2025_11_25)
            .expect("old protocol")
            .len(),
        1
    );
    let ClientJsonRpcMessage::Request(request) = &mut message else {
        panic!("request")
    };
    request
        .request
        .get_meta_mut()
        .set_protocol_version(ProtocolVersion::STANDARD_HEADERS);
    let headers = project(&message, None, &ProtocolVersion::V_2025_11_25).expect("request version");
    assert_eq!(value(&headers, "mcp-protocol-version"), Some("2026-07-28"));
    assert_eq!(value(&headers, "mcp-method"), Some("tools/call"));
    let message = ClientJsonRpcMessage::request(
        ClientRequest::CustomRequest(CustomRequest::new(
            "custom/method",
            Some(json!({"":"not a name","name":"also not a name"})),
        )),
        RequestId::Number(2),
    );
    let headers = project(&message, None, &ProtocolVersion::STANDARD_HEADERS).expect("custom");
    assert!(value(&headers, "mcp-name").is_none());
}

#[test]
fn exact_http_value_limit_is_checked_after_base64_expansion_and_before_allocation() {
    let input_schema = schema(&json!({"region":{"type":"string","x-mcp-header":"Region"}}));
    let annotations = ToolHeaderAnnotations::extract(&input_schema)
        .expect("admit")
        .expect("valid");
    let message = call(&json!({"region":"x".repeat(MAX_VALUE_BYTES)}));
    let headers = project(
        &message,
        Some(&annotations),
        &ProtocolVersion::STANDARD_HEADERS,
    )
    .expect("exact fit");
    assert_eq!(
        value(&headers, "mcp-param-region").expect("value").len(),
        MAX_VALUE_BYTES
    );
    let message = call(&json!({"region":"λ".repeat(MAX_VALUE_BYTES/2)}));
    assert!(
        project(
            &message,
            Some(&annotations),
            &ProtocolVersion::STANDARD_HEADERS
        )
        .is_err()
    );
    let mut properties = JsonObject::new();
    for index in 0..=MAX_HEADERS {
        properties.insert(
            format!("p{index}"),
            json!({"type":"string","x-mcp-header":format!("H{index}")}),
        );
    }
    assert!(ToolHeaderAnnotations::extract(&schema(&Value::Object(properties))).is_err());
}
