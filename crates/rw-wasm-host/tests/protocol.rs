#![allow(clippy::expect_used)]

use std::path::Path;

use rw_ext::{
    PROTOCOL_VERSION, PluginCapabilities, PluginHook, PluginHookDeclaration, PluginManifest,
    WasmHookLimits, WasmHostRequest, WasmHostResponse, invoke_helper,
};

fn manifest() -> PluginManifest {
    PluginManifest {
        name: "helper-test".to_owned(),
        version: "1.0.0".to_owned(),
        protocol: PROTOCOL_VERSION,
        capabilities: PluginCapabilities {
            hooks: vec![PluginHookDeclaration::Name(PluginHook::PreTool)],
            ..PluginCapabilities::default()
        },
    }
}

fn component(output: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(component
          (type $hook (func (param "event" string) (param "payload-json" string) (result string)))
          (core module $module
            (memory (export "memory") 1)
            (global $heap (mut i32) (i32.const 4096))
            (func (export "realloc") (param i32 i32 i32 i32) (result i32)
              (local $result i32)
              global.get $heap
              local.tee $result
              local.get 3
              i32.add
              global.set $heap
              local.get $result)
            (data (i32.const 256) "{}")
            (func (export "invoke") (param i32 i32 i32 i32) (result i32)
              i32.const 128
              i32.const 256
              i32.store
              i32.const 132
              i32.const {}
              i32.store
              i32.const 128))
          (core instance $instance (instantiate $module))
          (func $invoke (type $hook)
            (canon lift (core func $instance "invoke")
              (memory $instance "memory")
              (realloc (func $instance "realloc"))))
          (export "invoke" (func $invoke)))"#,
        output.replace('"', "\\22"),
        output.len()
    ))
    .expect("component WAT")
}

#[tokio::test]
async fn helper_validates_and_invokes_over_the_bounded_protocol() {
    let helper = Path::new(env!("CARGO_BIN_EXE_rottweiler-wasm-host"));
    let bytes = component(r#"{"directive":"replace","payload":{"safe":true}}"#);
    let limits = WasmHookLimits::default();
    assert_eq!(
        invoke_helper(
            helper,
            &WasmHostRequest::Validate {
                manifest: manifest(),
                limits,
            },
            &bytes,
        )
        .await
        .expect("validation response"),
        WasmHostResponse::Valid
    );
    assert_eq!(
        invoke_helper(
            helper,
            &WasmHostRequest::Invoke {
                manifest: manifest(),
                limits,
                event: "pre_tool".to_owned(),
                input: r#"{"tool":"read"}"#.to_owned(),
            },
            &bytes,
        )
        .await
        .expect("invocation response"),
        WasmHostResponse::Replace {
            payload: serde_json::json!({"safe": true}),
        }
    );
}

#[tokio::test]
async fn helper_rejects_malformed_components_without_crashing_the_client() {
    let response = invoke_helper(
        Path::new(env!("CARGO_BIN_EXE_rottweiler-wasm-host")),
        &WasmHostRequest::Validate {
            manifest: manifest(),
            limits: WasmHookLimits::default(),
        },
        b"not wasm",
    )
    .await
    .expect("typed rejection");
    assert!(matches!(response, WasmHostResponse::Error { .. }));
}
