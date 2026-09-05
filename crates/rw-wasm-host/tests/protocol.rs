#![allow(clippy::expect_used)]

use std::path::Path;

use rw_ext::{HookDispatcher, WasmHookLimits, WasmProcessHook, WasmWorkerPool};
use rw_plugin_protocol::{
    HookFailurePolicy, PROTOCOL_VERSION, PluginCapabilities, PluginHookCapability, PluginManifest,
};

fn manifest() -> PluginManifest {
    PluginManifest {
        name: "helper-test".to_owned(),
        version: "1.0.0".to_owned(),
        protocol: PROTOCOL_VERSION,
        capabilities: PluginCapabilities {
            hooks: vec![PluginHookCapability {
                name: rw_plugin_protocol::HookEvent::PreTool,
                class: rw_types::hook_contract::HookClass::Transform,
                failure_policy: HookFailurePolicy::FailOpen,
            }],
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
            (global $calls (mut i32) (i32.const 0))
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
              global.get $calls
              if unreachable end
              i32.const 1
              global.set $calls
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
async fn helper_reuses_compilation_with_fresh_invocations() {
    let pool = WasmWorkerPool::new();
    let hook = WasmProcessHook::new(
        pool.clone(),
        Path::new(env!("CARGO_BIN_EXE_rottweiler-wasm-host")).to_owned(),
        manifest(),
        component(r#"{"decision":"transform","change":{"hook":"pre_tool","name":"read","arguments":{"safe":true}}}"#),
        WasmHookLimits::default(),
    )
    .expect("proxy");
    hook.validate().await.expect("validation");
    let mut dispatcher = HookDispatcher::new();
    hook.register_hooks(&mut dispatcher).expect("registered");
    for _ in 0..10 {
        let result = dispatcher
            .dispatch(rw_ext::HookInput::PreTool(
                rw_types::hook_contract::HookToolInput {
                    id: "call".to_owned(),
                    name: "read".to_owned(),
                    arguments: serde_json::json!({}),
                },
            ))
            .await
            .expect("settled hook");
        let rw_ext::HookInput::PreTool(input) = result.input() else {
            panic!("pre_tool phase")
        };
        assert_eq!(input.arguments, serde_json::json!({"safe":true}));
        assert!(result.failures().is_empty());
    }
    assert_eq!(pool.stats().process_starts, 1);
    assert_eq!(pool.stats().component_loads, 1);
    assert_eq!(pool.stats().cache_hits, 10);
    assert!(pool.shutdown().await.is_ok());
}

#[tokio::test]
async fn helper_rejects_malformed_components_and_recovers() {
    let pool = WasmWorkerPool::new();
    let helper = Path::new(env!("CARGO_BIN_EXE_rottweiler-wasm-host")).to_owned();
    let invalid = WasmProcessHook::new(
        pool.clone(),
        helper.clone(),
        manifest(),
        b"not wasm".to_vec(),
        WasmHookLimits::default(),
    )
    .expect("bounded proxy");
    assert!(invalid.validate().await.is_err());
    let valid = WasmProcessHook::new(
        pool.clone(),
        helper,
        manifest(),
        component(r#"{"decision":"continue"}"#),
        WasmHookLimits::default(),
    )
    .expect("valid proxy");
    valid.validate().await.expect("replacement worker");
    assert_eq!(pool.stats().process_starts, 2);
    assert!(pool.shutdown().await.is_ok());
}

#[tokio::test]
async fn cache_is_bounded_and_manifest_and_limits_are_part_of_identity() {
    let pool = WasmWorkerPool::with_worker_limit(2).expect("capacity");
    let helper = Path::new(env!("CARGO_BIN_EXE_rottweiler-wasm-host")).to_owned();
    let bytes = component(r#"{"decision":"continue"}"#);
    let first = WasmProcessHook::new(
        pool.clone(),
        helper.clone(),
        manifest(),
        bytes.clone(),
        WasmHookLimits::default(),
    )
    .expect("first");
    let mut changed_manifest = manifest();
    changed_manifest.version = "2.0.0".to_owned();
    let second = WasmProcessHook::new(
        pool.clone(),
        helper.clone(),
        changed_manifest,
        bytes.clone(),
        WasmHookLimits::default(),
    )
    .expect("second");
    first.validate().await.expect("first compile");
    second.validate().await.expect("second compile");
    first.validate().await.expect("cached first");
    assert_eq!(pool.stats().process_starts, 2);
    assert_eq!(pool.stats().component_loads, 2);
    let limits = WasmHookLimits {
        fuel: 100_000,
        ..WasmHookLimits::default()
    };
    let third =
        WasmProcessHook::new(pool.clone(), helper, manifest(), bytes, limits).expect("third");
    third
        .validate()
        .await
        .expect("different fuel configuration");
    assert_eq!(pool.stats().process_starts, 2);
    assert_eq!(pool.stats().component_loads, 3);
    first.validate().await.expect("recent generation retained");
    assert_eq!(pool.stats().component_loads, 3);
    assert!(pool.shutdown().await.is_ok());
}

#[tokio::test]
async fn guest_trap_retires_its_worker_and_allows_a_fresh_generation() {
    let pool = WasmWorkerPool::with_worker_limit(1).expect("capacity");
    let helper = Path::new(env!("CARGO_BIN_EXE_rottweiler-wasm-host")).to_owned();
    let hook = WasmProcessHook::new(
        pool.clone(),
        helper,
        manifest(),
        component(r#"{"decision":"continue"}"#),
        WasmHookLimits {
            fuel: 1,
            ..WasmHookLimits::default()
        },
    )
    .expect("proxy");
    let mut dispatcher = HookDispatcher::new();
    hook.register_hooks(&mut dispatcher).expect("registered");
    for _ in 0..2 {
        let result = dispatcher
            .dispatch(rw_ext::HookInput::PreTool(
                rw_types::hook_contract::HookToolInput {
                    id: "call".to_owned(),
                    name: "read".to_owned(),
                    arguments: serde_json::json!({}),
                },
            ))
            .await
            .expect("settled hook");
        assert_eq!(result.failures().len(), 1);
    }
    assert_eq!(pool.stats().process_starts, 2);
    assert!(pool.shutdown().await.is_ok());
}

#[tokio::test]
#[ignore = "native worker-capacity measurement; run alone with a release helper"]
async fn worker_capacity_measurement() {
    use std::{sync::Arc, time::Instant};
    let helper = std::env::var_os("ROTTWEILER_WASM_BENCH_HELPER")
        .map(std::path::PathBuf::from)
        .expect("set ROTTWEILER_WASM_BENCH_HELPER to the release helper");
    for workers in [1, 2] {
        let pool = WasmWorkerPool::with_worker_limit(workers).expect("capacity");
        let hook = WasmProcessHook::new(
            pool.clone(),
            helper.clone(),
            manifest(),
            component(r#"{"decision":"continue"}"#),
            WasmHookLimits::default(),
        )
        .expect("proxy");
        let cold = Instant::now();
        let (first, second) = tokio::join!(hook.validate(), hook.validate());
        first.expect("cold first");
        second.expect("cold second");
        let cold_us = cold.elapsed().as_micros();
        let mut dispatcher = HookDispatcher::new();
        hook.register_hooks(&mut dispatcher).expect("registered");
        let dispatcher = Arc::new(dispatcher);
        let mut warm_us = Vec::new();
        for _ in 0..32 {
            let started = Instant::now();
            let result = dispatcher
                .dispatch(rw_ext::HookInput::PreTool(
                    rw_types::hook_contract::HookToolInput {
                        id: "call".to_owned(),
                        name: "read".to_owned(),
                        arguments: serde_json::json!({}),
                    },
                ))
                .await
                .expect("settled hook");
            warm_us.push(started.elapsed().as_micros());
            assert!(result.failures().is_empty());
        }
        let mut concurrent_us = Vec::new();
        for _ in 0..4 {
            let started = Instant::now();
            let mut jobs = Vec::new();
            for _ in 0..16 {
                let dispatcher = dispatcher.clone();
                jobs.push(tokio::spawn(async move {
                    dispatcher
                        .dispatch(rw_ext::HookInput::PreTool(
                            rw_types::hook_contract::HookToolInput {
                                id: "call".to_owned(),
                                name: "read".to_owned(),
                                arguments: serde_json::json!({}),
                            },
                        ))
                        .await
                        .expect("settled hook")
                }));
            }
            for job in jobs {
                assert!(job.await.expect("job").failures().is_empty());
            }
            concurrent_us.push(started.elapsed().as_micros());
        }
        let process_snapshot = std::process::Command::new("ps")
            .args(["-axo", "ppid=,rss=,comm="])
            .output()
            .expect("RSS sample");
        let own_pid = std::process::id().to_string();
        let rss_kib: u64 = String::from_utf8_lossy(&process_snapshot.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let parent = parts.next()?;
                let rss = parts.next()?.parse::<u64>().ok()?;
                let command = parts.collect::<Vec<_>>().join(" ");
                (parent == own_pid && command.ends_with("rottweiler-wasm-host")).then_some(rss)
            })
            .sum();
        println!(
            "{}",
            serde_json::json!({"workers":workers,"helper":helper,"cold_us":cold_us,"warm_us":warm_us,"concurrent_16_us":concurrent_us,"warm_workers_rss_kib":rss_kib,"process_starts":pool.stats().process_starts,"component_loads":pool.stats().component_loads,"cache_hits":pool.stats().cache_hits})
        );
        assert!(pool.shutdown().await.is_ok());
    }
}
