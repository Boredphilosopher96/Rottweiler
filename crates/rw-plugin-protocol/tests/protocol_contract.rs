#![allow(clippy::expect_used)]

use rw_plugin_protocol::{
    FrameDecoder, ManifestError, PluginManifest, ProviderHttpCapabilityParams,
    ProviderModelsResponse, RpcFrame, ToolCallParams, ToolProgressParams,
    validate_provider_alias_prefix,
};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/../../packages/plugin-sdk/fixtures/wire/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_slice(&std::fs::read(path).expect("fixture must be readable"))
        .expect("fixture must be JSON")
}

#[test]
fn fixture_deserializes_through_owned_wire_dtos() {
    let protocol_three = fixture("protocol-3.json");
    let models: ProviderModelsResponse =
        serde_json::from_value(protocol_three["provider_models_response"]["result"].clone())
            .expect("provider models result");
    assert_eq!(models.models[0].id, "vision-thinking");

    let http: ProviderHttpCapabilityParams =
        serde_json::from_value(protocol_three["provider_http_request"]["params"].clone())
            .expect("provider HTTP request");
    assert_eq!(http.credential_reference, "fixture-token");
    assert_eq!(http.request.body_base64.as_deref(), Some("e30="));

    let tool: ToolCallParams =
        serde_json::from_value(protocol_three["tool_call_request"]["params"].clone())
            .expect("typed tool lifetime");
    assert_eq!(tool.lifetime.total_ms(), 300_000);
    assert_eq!(tool.lifetime.idle_ms(), 90_000);
    let progress: ToolProgressParams =
        serde_json::from_value(protocol_three["tool_progress"]["params"].clone())
            .expect("typed tool progress");
    assert_eq!(progress.request_id, rw_plugin_protocol::RpcId::Number(17));
    assert_eq!(progress.sequence, 1);
    assert_eq!(progress.progress.message(), "working");
}

#[test]
fn independent_negative_samples_fail_at_the_owner_boundary() {
    let unknown_model_field = json!({
        "models": [{
            "id": "model",
            "capabilities": {
                "tool_calling": true,
                "vision": false,
                "thinking": false,
                "cache_breakpoints": "none"
            },
            "surprise": true
        }]
    });
    assert!(serde_json::from_value::<ProviderModelsResponse>(unknown_model_field).is_err());

    let invalid_manifest = json!({
        "name": "plugin",
        "version": "1.0.0",
        "protocol": 3,
        "capabilities": { "providers": [{ "alias-prefix": "Upper/" }] }
    });
    assert!(matches!(
        PluginManifest::from_slice(&serde_json::to_vec(&invalid_manifest).expect("manifest JSON")),
        Err(ManifestError::InvalidField {
            field: "providers.alias-prefix",
            ..
        })
    ));

    let shorthand_hook = json!({
        "name": "plugin",
        "version": "1.0.0",
        "protocol": 3,
        "capabilities": { "hooks": ["pre_tool"] }
    });
    assert!(
        PluginManifest::from_slice(
            &serde_json::to_vec(&shorthand_hook).expect("shorthand hook JSON")
        )
        .is_err()
    );

    let mut decoder = FrameDecoder::default();
    assert!(
        decoder
            .push(b"{\"jsonrpc\":\"1.0\",\"id\":1,\"method\":\"initialize\"}\n")
            .is_err()
    );
    assert_eq!(decoder.buffered_bytes(), 0);
    let valid = decoder
        .push(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
        .expect("valid frame");
    assert!(matches!(
        valid.as_slice(),
        [rw_plugin_protocol::DecodedFrame {
            frame: RpcFrame::Request(_),
            ..
        }]
    ));
}

#[test]
fn provider_alias_prefix_grammar_is_owned_and_bounded() {
    assert!(validate_provider_alias_prefix("a/").is_ok());
    assert!(validate_provider_alias_prefix(&format!("{}/", "a".repeat(127))).is_ok());
    for invalid in ["/", "Upper/", "missing", "bad+prefix/", "nonascii-é/"] {
        assert!(
            validate_provider_alias_prefix(invalid).is_err(),
            "unexpectedly accepted {invalid:?}"
        );
    }
    assert!(validate_provider_alias_prefix(&format!("{}/", "a".repeat(128))).is_err());
}

#[test]
fn decoder_preserves_original_byte_charges_across_numeric_and_escape_normalization() {
    for value in [
        "0.000001",
        "100000000000000000000",
        "1e-7",
        "1e+21",
        "-0",
        r#""\u0061\n\/é""#,
    ] {
        let line = format!(
            r#"{{"jsonrpc":"2.0","method":"provider/event","params":{{"request_id":1,"event":{{"type":"tool_call_end","arguments":{value}}}}}}}"#
        );
        let mut decoder = FrameDecoder::default();
        let input = format!("{line}\n");
        let mut frames = Vec::new();
        for chunk in input.as_bytes().chunks(7) {
            frames.extend(decoder.push(chunk).expect("wire frame"));
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].wire_bytes, line.len());
        // Byte credit is independent of serde's canonical numeric/string spelling.
        let serialized = serde_json::to_vec(&frames[0].frame).expect("reserialize");
        if value == "0.000001" || value == "100000000000000000000" {
            assert_ne!(serialized.len(), line.len());
        }
    }
}

#[test]
fn initialization_has_one_exact_protocol_and_no_range_fields() {
    let params = fixture("protocol-3.json")["initialize_request"]["params"].clone();
    let parsed: rw_plugin_protocol::InitializeParams =
        serde_json::from_value(params.clone()).expect("initialize params");
    assert_eq!(parsed.protocol, rw_plugin_protocol::PROTOCOL_VERSION);
    let mut ranged = params;
    ranged["min_protocol"] = json!(rw_plugin_protocol::PROTOCOL_VERSION);
    assert!(serde_json::from_value::<rw_plugin_protocol::InitializeParams>(ranged).is_err());
    for protocol in [0, 2, 4, u32::MAX] {
        let bytes = serde_json::to_vec(&json!({
            "name": "plugin", "version": "1", "protocol": protocol, "capabilities": {}
        }))
        .expect("manifest");
        assert_eq!(
            PluginManifest::from_slice(&bytes).expect_err("protocol identity"),
            ManifestError::UnsupportedProtocol {
                protocol,
                expected: rw_plugin_protocol::PROTOCOL_VERSION,
            }
        );
    }
}

#[test]
fn provider_http_requires_its_host_invocation_identity() {
    let mut params = fixture("protocol-3.json")["provider_http_request"]["params"].clone();
    params
        .as_object_mut()
        .expect("HTTP params")
        .remove("invocation_id");
    assert!(serde_json::from_value::<ProviderHttpCapabilityParams>(params).is_err());
}
