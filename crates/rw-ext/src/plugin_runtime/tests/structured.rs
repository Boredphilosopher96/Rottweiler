use super::*;
use crate::plugin::PluginProviderEventStream;
use rw_providers::{OutputContract, OutputField, OutputSchema};
struct Client {
    calls: AtomicUsize,
    valid: bool,
}
#[async_trait]
impl PluginRpcClient for Client {
    async fn call_command(
        &self,
        _: rw_plugin_protocol::CommandExecuteParams,
        _: &rw_tools::CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        Err(PluginRpcError {
            code: "unsupported".into(),
            message: "not a command fixture".into(),
        })
    }
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        Ok(())
    }
    async fn request(&self, method: &str, _: Value) -> Result<Value, PluginRpcError> {
        assert_eq!(method, METHOD_PROVIDER_MODELS);
        Ok(
            json!({"models":[{"id":"model","capabilities":{"structured_output":"json_schema","tool_calling":false,"vision":false,"thinking":false,"cache_breakpoints":"none"}}]}),
        )
    }
    async fn provider_stream(
        &self,
        params: Value,
    ) -> Result<PluginProviderEventStream, PluginRpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(params["request"]["output"]["mode"], "json_schema");
        let text = if self.valid {
            r#"{"ok":true}"#
        } else {
            r#"{"ok":"wrong"}"#
        };
        Ok(Box::pin(futures_util::stream::iter([
            Ok(json!({"type":"text_delta","text":text})),
            Ok(json!({"type":"finished","reason":"stop"})),
        ])))
    }
}
fn request() -> ProviderRequest {
    ProviderRequest {
        model: "model".into(),
        turns: vec![],
        tools: vec![],
        tool_choice: ToolChoice::None {},
        output: OutputContract::JsonSchema {
            name: "result".into(),
            schema: OutputSchema::Object {
                fields: vec![OutputField {
                    name: "ok".into(),
                    schema: OutputSchema::Boolean {},
                }],
            },
        },
        max_output_tokens: 100,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    }
}
#[tokio::test]
async fn rpc_structured_output_requires_advertisement_and_validates_the_completed_body() {
    for valid in [false, true] {
        let mut approved = manifest();
        approved.capabilities.providers[0].capabilities = vec!["models".into()];
        let process: Arc<dyn SupervisedPluginProcess> = Arc::new(FakeProcess::default());
        let enforcer = Arc::new(CapabilityEnforcer::new(&approved, process));
        let client = Arc::new(Client {
            calls: AtomicUsize::new(0),
            valid,
        });
        let adapter = RpcProviderAdapter::new(
            "fixture",
            "fixture/",
            Capabilities {
                tool_calling: false,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            },
            crate::plugin_endpoint::fixture_endpoint(approved, client.clone(), enforcer),
        )
        .with_model_catalog();
        let Err(error) = adapter.stream(request()).await else {
            panic!("unadvertised support")
        };
        assert_eq!(error.kind, ProviderErrorKind::Unsupported);
        assert_eq!(client.calls.load(Ordering::SeqCst), 0);
        adapter.discover_models().await.expect("catalog");
        let events: Vec<_> = adapter
            .stream(request())
            .await
            .expect("admission")
            .collect()
            .await;
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, Ok(ProviderEvent::Finished { .. }))),
            valid
        );
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, Ok(ProviderEvent::TextDelta { .. }))),
            valid
        );
        assert_eq!(events.iter().any(Result::is_err), !valid);
        adapter.settle_effects().await.expect("effect settlement");
    }
}
