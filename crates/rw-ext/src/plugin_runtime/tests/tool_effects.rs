use super::*;
use crate::tool_effects::{ToolEffectsCall, ToolEffectsOwner};
use rw_tools::{
    CapabilityManifest, DelegatedTools, MutationScope, ReadTool, ToolContext, ToolEffectGrant,
    ToolLimits,
};

struct EffectFixture {
    client: Arc<JsonRpcPluginClient>,
    input: BufReader<tokio::io::DuplexStream>,
    output: tokio::io::DuplexStream,
    call: ToolEffectsCall,
    _root: TempDir,
}
impl EffectFixture {
    fn new() -> Self {
        let root = TempDir::new().expect("workspace");
        std::fs::write(root.path().join("file"), "scoped file bytes").expect("input");
        let mut tools = ToolRegistry::default();
        tools
            .register(Arc::new(ReadTool::new(ToolLimits::default())))
            .expect("read tool");
        let capabilities = CapabilityManifest::new([ToolCapability::ReadFilesystem]);
        let host = Arc::new(DelegatedTools::new(
            ToolContext::new(root.path()).expect("context"),
            Arc::new(tools),
            capabilities.clone(),
            MutationScope::None,
        ));
        let owner = Arc::new(ToolEffectsOwner::default());
        let call = owner
            .begin(
                host,
                ToolEffectGrant::new(capabilities, &[]).expect("grant"),
            )
            .expect("owned tool effect");
        let process = Arc::new(FakeProcess::default());
        let (host_stdin, plugin_input) = tokio::io::duplex(64 * 1024);
        let (plugin_output, host_stdout) = tokio::io::duplex(64 * 1024);
        let mut manifest = manifest();
        manifest.capabilities.push.push(PluginPush::EffectToolCall);
        let client = JsonRpcPluginClient::start(
            LaunchedPluginProcess {
                stdin: Box::pin(host_stdin),
                stdout: Box::pin(BufReader::new(host_stdout)),
                stderr: Box::pin(BufReader::new(tokio::io::empty())),
                process: process.clone(),
                executable_identity: shell_config(&root).executable_identity().clone(),
            },
            Arc::new(CapabilityEnforcer::new(&manifest, process)),
            Arc::new(DenyPushHandler),
            Arc::new(DenyPluginProviderHttpHandler),
            Arc::new(NoopPluginBoundaryRedactor),
            Duration::from_secs(1),
        );
        Self {
            client,
            input: BufReader::new(plugin_input),
            output: plugin_output,
            call,
            _root: root,
        }
    }
    async fn send(&mut self, frame: Value) {
        self.output
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .expect("plugin frame");
    }
    async fn receive(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), self.input.read_line(&mut line))
            .await
            .expect("bounded response")
            .expect("host frame");
        serde_json::from_str(&line).expect("typed JSON")
    }
}

#[tokio::test]
async fn nested_file_effects_require_the_exact_pending_tool_request() {
    let mut fixture = EffectFixture::new();
    let request = tokio::spawn({
        let client = fixture.client.clone();
        let effects = fixture.call.effects();
        async move {
            client
                .call_tool(
                    ToolCallParams {
                        name: "fixture_tool".into(),
                        input: json!({}),
                        lifetime: rw_plugin_protocol::OperationLifetime::default(),
                    },
                    &CancellationToken::default(),
                    Arc::new(rw_tools::NoopProgressSink),
                    Some(effects),
                )
                .await
        }
    });
    let outbound = fixture.receive().await;
    let request_id = outbound["id"].as_u64().expect("host request identity");
    fixture.send(json!({"jsonrpc":"2.0", "id":"wrong", "method":rw_plugin_protocol::METHOD_EFFECT_TOOL_CALL,
        "params":{"request_id":request_id+1,"name":"read","input":{"path":"file", "line_count":null}}})).await;
    let rejected = fixture.receive().await;
    assert_eq!(rejected["error"]["data"]["code"], "effect_denied");
    fixture.send(json!({"jsonrpc":"2.0", "id":"read", "method":rw_plugin_protocol::METHOD_EFFECT_TOOL_CALL,
        "params":{"request_id":request_id,"name":"read","input":{"path":"file", "line_count":null}}})).await;
    let response = fixture.receive().await;
    assert_eq!(response["id"], "read");
    assert!(
        response["result"]["content"]
            .as_str()
            .expect("read content")
            .contains("scoped file bytes")
    );
    fixture.send(json!({"jsonrpc":"2.0", "id":request_id, "result":{"content":"complete","data":null,"truncated":false}})).await;
    request.await.expect("RPC owner").expect("plugin result");
    fixture.send(json!({"jsonrpc":"2.0", "id":"late", "method":rw_plugin_protocol::METHOD_EFFECT_TOOL_CALL,
        "params":{"request_id":request_id,"name":"read","input":{"path":"file", "line_count":null}}})).await;
    assert_eq!(
        fixture.receive().await["error"]["data"]["code"],
        "effect_denied"
    );
    fixture
        .call
        .finish()
        .await
        .expect("host effect scope settled");
}
