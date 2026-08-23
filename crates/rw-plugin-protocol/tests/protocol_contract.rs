#![allow(clippy::expect_used)]

use rw_plugin_protocol::{
    FrameDecoder, ManifestError, PluginManifest, ProviderHttpCapabilityParams,
    ProviderModelsResponse, RpcFrame, validate_provider_alias_prefix,
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
fn current_fixture_deserializes_through_owned_wire_dtos() {
    let protocol_two = fixture("protocol-2.json");
    let models: ProviderModelsResponse =
        serde_json::from_value(protocol_two["provider_models_response"]["result"].clone())
            .expect("provider models result");
    assert_eq!(models.models[0].id, "vision-thinking");

    let http: ProviderHttpCapabilityParams =
        serde_json::from_value(protocol_two["provider_http_request"]["params"].clone())
            .expect("provider HTTP request");
    assert_eq!(http.credential_reference, "fixture-token");
    assert_eq!(http.request.body_base64.as_deref(), Some("e30="));
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
        "protocol": 2,
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
        "protocol": 2,
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
    assert!(matches!(valid.as_slice(), [RpcFrame::Request(_)]));
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
