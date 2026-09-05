#![allow(clippy::expect_used)]

mod authority;
mod catalog;
mod sdk;
mod settlement;
mod state;
mod transport;

use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;

use futures_util::StreamExt;
use rw_providers::{CacheBreakpointSupport, ToolChoice, WireMode};
use rw_tools::ToolRegistry;
use rw_types::config::ThinkingLevel;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::plugin::{ApprovalStoreError, CapabilityViolation};
use rw_plugin_protocol::{
    HookFailurePolicy, PluginCommandCapability, PluginHookCapability, PluginProviderCapability,
    PluginPush, PluginToolCapability, PluginToolEffect,
};

fn manifest() -> PluginManifest {
    PluginManifest {
        name: "runtime-fixture".to_owned(),
        version: "1.0.0".to_owned(),
        protocol: rw_plugin_protocol::PROTOCOL_VERSION,
        capabilities: PluginCapabilities {
            tools: vec![PluginToolCapability {
                name: "fixture_tool".to_owned(),
                description: "fixture tool".to_owned(),
                schema: json!({"type":"object"}),
                caps: vec![PluginToolEffect::ReadsFilesystem],
            }],
            commands: vec![PluginCommandCapability {
                name: "fixture".to_owned(),
                description: "fixture command".to_owned(),
                argument_hint: None,
                allowed_tools: Vec::new(),
            }],
            hooks: vec![PluginHookCapability {
                name: rw_plugin_protocol::HookEvent::PreTool,
                class: rw_types::hook_contract::HookClass::Transform,
                failure_policy: HookFailurePolicy::FailOpen,
            }],
            providers: vec![PluginProviderCapability {
                alias_prefix: "fixture/".to_owned(),
                capabilities: Vec::new(),
                credential_references: Vec::new(),
            }],
            event_subscriptions: vec![rw_plugin_protocol::ExtensionEventKind::TurnFinished],
            push: vec![PluginPush::UiNotify],
        },
    }
}

struct CatalogClient(Value);

#[async_trait]
impl PluginRpcClient for CatalogClient {
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        Ok(())
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value, PluginRpcError> {
        assert_eq!(method, METHOD_PROVIDER_MODELS);
        assert_eq!(params, json!({"alias_prefix":"fixture/"}));
        Ok(self.0.clone())
    }
}

fn catalog_adapter(value: Value) -> RpcProviderAdapter {
    let mut approved = manifest();
    approved.protocol = rw_plugin_protocol::PROTOCOL_VERSION;
    approved.capabilities.providers[0].capabilities = vec!["models".to_owned()];
    let process: Arc<dyn SupervisedPluginProcess> = Arc::new(FakeProcess::default());
    let enforcer = Arc::new(CapabilityEnforcer::new(&approved, process));
    RpcProviderAdapter::new(
        "catalog-fixture",
        "fixture/",
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        },
        crate::plugin_endpoint::fixture_endpoint(
            approved,
            Arc::new(CatalogClient(value)),
            enforcer,
        ),
    )
    .with_model_catalog()
}

#[derive(Default)]
struct MemoryApproval(StdMutex<BTreeMap<String, String>>);

impl ApprovalStore for MemoryApproval {
    fn approved_fingerprint(&self, name: &str) -> Result<Option<String>, ApprovalStoreError> {
        Ok(self.0.lock().expect("approval lock").get(name).cloned())
    }
    fn record_approval(&self, name: &str, fingerprint: &str) -> Result<(), ApprovalStoreError> {
        self.0
            .lock()
            .expect("approval lock")
            .insert(name.to_owned(), fingerprint.to_owned());
        Ok(())
    }
}

#[derive(Default)]
struct FakeProcess {
    killed: AtomicUsize,
    waited: AtomicUsize,
    violations: StdMutex<Vec<CapabilityViolation>>,
    kill_fails: AtomicBool,
    settlement_blocked: AtomicBool,
    settlement_release: tokio::sync::Notify,
}

#[async_trait]
impl SupervisedPluginProcess for FakeProcess {
    async fn settle_effects(&self) -> Result<(), PluginProcessError> {
        if self.settlement_blocked.load(Ordering::Acquire) {
            self.settlement_release.notified().await;
        }
        self.reap().await
    }
    fn mark_capability_violation(&self, violation: &CapabilityViolation) {
        self.violations
            .lock()
            .expect("violation lock")
            .push(violation.clone());
    }
    fn kill_tree(&self) -> Result<(), PluginProcessError> {
        self.killed.fetch_add(1, Ordering::AcqRel);
        if self.kill_fails.load(Ordering::Acquire) {
            Err(PluginProcessError {
                message: "seeded kill failure".to_owned(),
            })
        } else {
            Ok(())
        }
    }
    async fn wait(&self) -> Result<Option<i32>, PluginProcessError> {
        self.waited.fetch_add(1, Ordering::AcqRel);
        Ok(Some(0))
    }
}

struct MemoryLauncher {
    manifest: PluginManifest,
    process: Arc<FakeProcess>,
    push: Option<String>,
    hang_method: Option<String>,
}

#[derive(Default)]
struct TrackingDirectLauncher(StdMutex<Option<Arc<dyn SupervisedPluginProcess>>>);

#[async_trait]
impl PluginLauncher for TrackingDirectLauncher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginLaunchError> {
        let launched = TestDirectLauncher.launch(config, profile).await?;
        *self.0.lock().expect("tracking launcher") = Some(Arc::clone(&launched.process));
        Ok(launched)
    }
}

#[derive(Default)]
struct RecordingPush(StdMutex<Vec<(String, Value)>>);

#[async_trait]
impl PushHandler for RecordingPush {
    async fn handle_push(&self, method: &str, params: Value) -> Result<Value, PluginRpcError> {
        self.0
            .lock()
            .expect("push lock")
            .push((method.to_owned(), params));
        Ok(Value::Null)
    }
}

struct CanaryRedactor;

impl PluginBoundaryRedactor for CanaryRedactor {
    fn redact(&self, mut value: Value) -> Value {
        fn visit(value: &mut Value) {
            match value {
                Value::String(text) => {
                    *text = text.replace("PLUGIN_CANARY_SECRET", "[REDACTED]");
                }
                Value::Array(values) => values.iter_mut().for_each(visit),
                Value::Object(values) => values.values_mut().for_each(visit),
                Value::Null | Value::Bool(_) | Value::Number(_) => {}
            }
        }
        visit(&mut value);
        value
    }
}

const HTTP_SECRET: &str = "PLUGIN_HTTP_SECRET_CANARY";

struct HttpSecretRedactor;

impl PluginBoundaryRedactor for HttpSecretRedactor {
    fn redact(&self, mut value: Value) -> Value {
        fn visit(value: &mut Value) {
            match value {
                Value::String(text) => *text = text.replace(HTTP_SECRET, "[REDACTED]"),
                Value::Array(values) => values.iter_mut().for_each(visit),
                Value::Object(values) => values.values_mut().for_each(visit),
                Value::Null | Value::Bool(_) | Value::Number(_) => {}
            }
        }
        visit(&mut value);
        value
    }

    fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        String::from_utf8_lossy(value)
            .replace(HTTP_SECRET, "[REDACTED]")
            .into_bytes()
    }

    fn redact_streaming_prefix(&self, value: &[u8], retain: usize) -> (Vec<u8>, Vec<u8>) {
        let redactor = rw_providers::FixtureRedactor::new([HTTP_SECRET.to_owned()]);
        redactor.redact_streaming_prefix(value, retain)
    }

    fn maximum_secret_bytes(&self) -> usize {
        HTTP_SECRET.len()
    }
}

#[derive(Default)]
struct FixtureProviderHttp {
    requests: StdMutex<Vec<Value>>,
    cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl PluginProviderHttpHandler for FixtureProviderHttp {
    async fn request(
        &self,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<PluginHttpStreamResponse, PluginRpcError> {
        if cancellation.is_cancelled() {
            return Err(rpc_error(
                "cancelled",
                "fixture provider HTTP was cancelled",
            ));
        }
        self.requests.lock().expect("request lock").push(params);
        let is_cancellation_fixture = self
            .requests
            .lock()
            .expect("request lock")
            .last()
            .and_then(|params| params.pointer("/request/url"))
            .and_then(Value::as_str)
            .is_some_and(|url| url.ends_with("/cancelled"));
        if is_cancellation_fixture {
            let cancellation = cancellation.clone();
            let cancelled = Arc::clone(&self.cancelled);
            tokio::spawn(async move {
                cancellation.cancelled().await;
                cancelled.store(true, Ordering::Release);
            });
            return Ok(PluginHttpStreamResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::pin(futures_util::stream::pending()),
            });
        }
        let wire = format!(
            "{{\"type\":\"message_start\",\"model\":\"tool-model\"}}\n\
                 {{\"type\":\"tool_call_start\",\"id\":\"call-1\",\"name\":\"lookup\"}}\n\
                 {{\"type\":\"tool_call_arguments_delta\",\"id\":\"call-1\",\"json_fragment\":\"{{\\\"city\\\":\\\"Chicago\\\"}}\"}}\n\
                 {{\"type\":\"tool_call_end\",\"id\":\"call-1\",\"arguments\":{{\"city\":\"Chicago\"}}}}\n\
                 {{\"type\":\"text_delta\",\"text\":\"{HTTP_SECRET}\"}}\n\
                 {{\"type\":\"finished\",\"reason\":\"tool_calls\"}}\n"
        );
        let split = wire.find(HTTP_SECRET).expect("secret marker") + 8;
        let chunks = vec![
            Ok(wire.as_bytes()[..split].to_vec()),
            Ok(wire.as_bytes()[split..].to_vec()),
        ];
        Ok(PluginHttpStreamResponse {
            status: 200,
            headers: vec![("x-echo".to_owned(), HTTP_SECRET.to_owned())],
            body: Box::pin(futures_util::stream::iter(chunks)),
        })
    }
}

#[async_trait]
impl PluginLauncher for MemoryLauncher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginLaunchError> {
        config
            .validate_executable_identity()
            .map_err(PluginLaunchError::Rejected)?;
        assert_eq!(profile.capabilities, self.manifest.capabilities);
        let (host_stdin, plugin_input) = tokio::io::duplex(64 * 1024);
        let (plugin_output, host_stdout) = tokio::io::duplex(64 * 1024);
        let manifest = self.manifest.clone();
        let push = self.push.clone();
        let hang_method = self.hang_method.clone();
        tokio::spawn(async move {
            let mut input = BufReader::new(plugin_input);
            let mut output = plugin_output;
            let mut line = String::new();
            while input.read_line(&mut line).await.expect("fixture read") != 0 {
                let frame: RpcFrame = serde_json::from_str(line.trim_end()).expect("host frame");
                line.clear();
                match frame {
                    RpcFrame::Request(request) if request.method == METHOD_INITIALIZE => {
                        if let Some(method) = push.as_deref() {
                            let push = RpcFrame::Request(RpcRequest {
                                jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                                id: RpcId::String("push-1".to_owned()),
                                method: method.to_owned(),
                                params: Some(json!({"message":"hello"})),
                            });
                            output
                                .write_all(
                                    &encode_frame(&push, MAX_FRAME_BYTES).expect("push frame"),
                                )
                                .await
                                .expect("push write");
                        }
                        let response = RpcFrame::Success(RpcSuccess {
                            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                            id: request.id,
                            result: serde_json::to_value(&manifest).expect("manifest"),
                        });
                        output
                            .write_all(
                                &encode_frame(&response, MAX_FRAME_BYTES).expect("response frame"),
                            )
                            .await
                            .expect("response write");
                    }
                    RpcFrame::Request(request)
                        if hang_method.as_deref() == Some(&request.method) => {}
                    RpcFrame::Request(request) if request.method == METHOD_TOOL_CALL => {
                        let response = RpcFrame::Success(RpcSuccess {
                            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                            id: request.id,
                            result: serde_json::to_value(ToolResult::new(
                                "fixture",
                                json!({"ok":true}),
                            ))
                            .expect("tool result"),
                        });
                        output
                            .write_all(
                                &encode_frame(&response, MAX_FRAME_BYTES).expect("response frame"),
                            )
                            .await
                            .expect("response write");
                    }
                    RpcFrame::Request(request) if request.method == METHOD_SHUTDOWN => {
                        let response = RpcFrame::Success(RpcSuccess {
                            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                            id: request.id,
                            result: Value::Null,
                        });
                        output
                            .write_all(
                                &encode_frame(&response, MAX_FRAME_BYTES).expect("response frame"),
                            )
                            .await
                            .expect("response write");
                    }
                    RpcFrame::Notification(notification) if notification.method == METHOD_EXIT => {
                        break;
                    }
                    _ => {}
                }
            }
        });
        Ok(LaunchedPluginProcess {
            stdin: Box::pin(host_stdin),
            stdout: Box::pin(BufReader::new(host_stdout)),
            stderr: Box::pin(BufReader::new(tokio::io::empty())),
            process: self.process.clone(),
            executable_identity: config.executable_identity().clone(),
        })
    }
}

fn shell_config(root: &TempDir) -> PluginProcessConfig {
    PluginProcessConfig::new(PathBuf::from("/bin/sh"))
        .expect("shell config")
        .with_cwd(root.path())
        .expect("cwd")
}

async fn mutating_child_client(root: &TempDir, timeout: Duration) -> Arc<JsonRpcPluginClient> {
    let config = shell_config(root).with_argv([
        "-c",
        "read request; (while :; do printf child >> child-writes; /bin/sleep 0.01; done) & printf ready > ready; while :; do printf parent >> parent-writes; /bin/sleep 0.01; done",
    ]).expect("fixture argv");
    let mut approved = manifest();
    approved.capabilities.tools[0]
        .caps
        .push(PluginToolEffect::WritesFilesystem);
    let launched = TestDirectLauncher
        .launch(
            &config,
            &PluginSandboxProfile {
                mode: PluginSandboxMode::Approved,
                capabilities: approved.capabilities.clone(),
                approved_roots: vec![root.path().to_path_buf()],
                allowed_domains: Vec::new(),
            },
        )
        .await
        .expect("fixture process");
    let enforcer = Arc::new(CapabilityEnforcer::new(
        &approved,
        Arc::clone(&launched.process),
    ));
    JsonRpcPluginClient::start(
        launched,
        enforcer,
        Arc::new(DenyPushHandler),
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(NoopPluginBoundaryRedactor),
        timeout,
    )
}

async fn wait_for_mutation(root: &TempDir) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !root.path().join("ready").exists()
            || !root.path().join("parent-writes").exists()
            || !root.path().join("child-writes").exists()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("plugin and child started writing");
}

async fn assert_conflicting_writes_are_safe(root: &TempDir) {
    for name in ["parent-writes", "child-writes"] {
        std::fs::write(root.path().join(name), b"subsequent mutation").expect("conflicting write");
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    for name in ["parent-writes", "child-writes"] {
        assert_eq!(
            std::fs::read(root.path().join(name)).expect("settled output"),
            b"subsequent mutation"
        );
    }
}

#[derive(Default)]
struct DelayedActorPush {
    panic_after_admission: bool,
    started: tokio::sync::Notify,
    release: Arc<tokio::sync::Notify>,
    committed: Arc<AtomicBool>,
}

#[async_trait]
impl PushHandler for DelayedActorPush {
    async fn handle_push(&self, _method: &str, _params: Value) -> Result<Value, PluginRpcError> {
        let release = Arc::clone(&self.release);
        let committed = Arc::clone(&self.committed);
        let (reply, outcome) = oneshot::channel();
        tokio::spawn(async move {
            release.notified().await;
            committed.store(true, Ordering::Release);
            let _ = reply.send(());
        });
        self.started.notify_one();
        assert!(
            !self.panic_after_admission,
            "fixture owner panicked after actor admission"
        );
        outcome.await.expect("actor outcome");
        Ok(Value::Null)
    }
}

#[derive(Default)]
struct IgnoringCancellationHttp {
    started: tokio::sync::Notify,
    dropped: Arc<AtomicBool>,
}

#[async_trait]
impl PluginProviderHttpHandler for IgnoringCancellationHttp {
    async fn request(
        &self,
        _params: Value,
        _cancellation: &CancellationToken,
    ) -> Result<PluginHttpStreamResponse, PluginRpcError> {
        struct MarkDropped(Arc<AtomicBool>);
        impl Drop for MarkDropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _dropped = MarkDropped(Arc::clone(&self.dropped));
        self.started.notify_one();
        std::future::pending().await
    }
}

fn bun_executable() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is available for Bun conformance");
    std::env::split_paths(&path)
        .map(|entry| entry.join("bun"))
        .find(|candidate| candidate.is_file())
        .expect("Bun is required for plugin conformance")
}

fn sdk_fixture_config(name: &str) -> (PathBuf, PluginProcessConfig) {
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/plugin-sdk")
        .canonicalize()
        .expect("SDK path");
    let fixture = sdk.join("fixtures/conformance").join(name);
    let config = PluginProcessConfig::new(bun_executable())
        .expect("Bun config")
        .with_argv([fixture.into_os_string()])
        .expect("fixture argv")
        .with_cwd(&sdk)
        .expect("SDK cwd");
    (sdk, config)
}

async fn approved_fixture_host(
    config: &PluginProcessConfig,
    root: &std::path::Path,
    push: Arc<dyn PushHandler>,
) -> Arc<PluginHost> {
    approved_fixture_host_with_http(
        config,
        root,
        push,
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(NoopPluginBoundaryRedactor),
    )
    .await
}

async fn approved_fixture_host_with_http(
    config: &PluginProcessConfig,
    root: &std::path::Path,
    push: Arc<dyn PushHandler>,
    provider_http: Arc<dyn PluginProviderHttpHandler>,
    redactor: Arc<dyn PluginBoundaryRedactor>,
) -> Arc<PluginHost> {
    let manifest = probe_plugin_manifest(
        &TestDirectLauncher,
        config,
        &[root.to_path_buf()],
        Arc::new(NoopPluginBoundaryRedactor),
    )
    .await
    .expect("probe fixture manifest");
    let store = MemoryApproval::default();
    approve_plugin_launch(&store, &manifest, config, "conformance:typescript")
        .expect("approve fixture");
    Arc::new(
        PluginHost::launch_approved_with_http(
            &TestDirectLauncher,
            &store,
            config,
            "conformance:typescript",
            &[root.to_path_buf()],
            manifest,
            push,
            provider_http,
            redactor,
        )
        .await
        .expect("launch approved fixture"),
    )
}

fn ready_endpoint(host: &Arc<PluginHost>) -> Arc<dyn crate::PluginEndpoint> {
    Arc::new(crate::ReadyPluginEndpoint::new(Arc::clone(host)).expect("ready endpoint"))
}
