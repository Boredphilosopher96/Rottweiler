use super::*;
use crate::PluginEndpoint;

pub struct RpcToolAdapter {
    declaration: rw_plugin_protocol::PluginToolCapability,
    endpoint: Arc<dyn PluginEndpoint>,
    presentation: Option<Arc<rw_types::extension_ui::UiContribution>>,
    effects: Arc<crate::tool_effects::ToolEffectsOwner>,
}

impl RpcToolAdapter {
    /// Constructs an adapter only for the exact immutable endpoint declaration.
    ///
    /// # Errors
    ///
    /// Returns an approval error if any declaration field differs from the manifest snapshot.
    pub fn new(
        declaration: rw_plugin_protocol::PluginToolCapability,
        endpoint: Arc<dyn PluginEndpoint>,
    ) -> Result<Self, PluginHostError> {
        if !endpoint.metadata().tool_declaration_matches(&declaration) {
            return Err(PluginHostError::Approval(
                "tool adapter declaration differs from endpoint manifest".to_owned(),
            ));
        }
        let presentation=endpoint.metadata().manifest().capabilities.ui.iter().find(|contribution|matches!(contribution,rw_types::extension_ui::UiContribution::Tool{tool_name,..} if tool_name==&declaration.name)).map(|declaration|Arc::new(declaration.clone()));
        Ok(Self {
            declaration,
            endpoint,
            presentation,
            effects: Arc::default(),
        })
    }
}

#[async_trait]
impl Tool for RpcToolAdapter {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        let (endpoint, effects) =
            tokio::join!(self.endpoint.settle_effects(), self.effects.settle(),);
        endpoint
            .map_err(|error| ToolError::EffectsUnsettled(error.to_string()))
            .and(effects)
    }

    fn delegates_effects(&self) -> bool {
        true
    }
    fn descriptor(&self) -> ToolDescriptor {
        let process_effects = self.endpoint.metadata().process_tool_effects();
        ToolDescriptor {
            name: self.declaration.name.clone(),
            description: self.declaration.description.clone(),
            input_schema: self.declaration.schema.clone(),
            capabilities: CapabilityManifest::new(process_effects.iter().copied().map(tool_effect)),
        }
    }

    fn mutation_scope(&self, _input: &Value) -> MutationScope {
        if self
            .endpoint
            .metadata()
            .process_tool_effects()
            .contains(&rw_plugin_protocol::PluginToolEffect::WritesFilesystem)
        {
            MutationScope::OpaqueWorkspace
        } else {
            MutationScope::None
        }
    }

    async fn execute(&self, _context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let connection = self
            .endpoint
            .connect(&_context.cancellation)
            .await
            .map_err(|error| ToolError::Output(error.to_string()))?;
        connection
            .enforcer()
            .check_tool(&self.declaration.name)
            .map_err(|error| ToolError::Output(error.to_string()))?;
        let effects = _context
            .effect_host()
            .map(|host| {
                let grant = rw_tools::ToolEffectGrant::new(
                    CapabilityManifest::new(self.declaration.caps.iter().copied().map(tool_effect)),
                    connection.effect_domains(),
                )?;
                self.effects.begin(host, grant)
            })
            .transpose()?;
        let result = connection
            .client()
            .call_tool(
                ToolCallParams {
                    name: self.declaration.name.clone(),
                    input,
                    lifetime: rw_plugin_protocol::OperationLifetime::default(),
                },
                &_context.cancellation,
                Arc::clone(&_context.progress),
                effects
                    .as_ref()
                    .map(crate::tool_effects::ToolEffectsCall::effects),
            )
            .await;
        if let Some(effects) = effects {
            effects.finish().await?;
        }
        let result = result.map_err(|error| ToolError::Output(error.to_string()))?;
        let result: ToolResult = serde_json::from_value(result).map_err(|error| {
            ToolError::Output(format!("plugin returned invalid tool result: {error}"))
        })?;
        let metadata = self.endpoint.metadata();
        if let Some(declaration) = &self.presentation {
            let presentation =
                rw_tools::ToolPresentationPlan::new(metadata.ui_owner(), Arc::clone(declaration))
                    .map_err(|error| ToolError::Output(error.to_string()))?;
            Ok(result.with_presentation(presentation))
        } else {
            Ok(result)
        }
    }
}

fn tool_effect(effect: rw_plugin_protocol::PluginToolEffect) -> ToolCapability {
    match effect {
        rw_plugin_protocol::PluginToolEffect::ReadsFilesystem => ToolCapability::ReadFilesystem,
        rw_plugin_protocol::PluginToolEffect::WritesFilesystem => ToolCapability::WriteFilesystem,
        rw_plugin_protocol::PluginToolEffect::Network => ToolCapability::Network,
        rw_plugin_protocol::PluginToolEffect::Execute => ToolCapability::Execute,
    }
}

pub struct RpcCommandAdapter {
    name: String,
    endpoint: Arc<dyn PluginEndpoint>,
}

impl RpcCommandAdapter {
    #[must_use]
    pub fn new(name: impl Into<String>, endpoint: Arc<dyn PluginEndpoint>) -> Self {
        Self {
            name: name.into(),
            endpoint,
        }
    }
}

#[async_trait]
impl<Context> CommandHandler<Context, Value> for RpcCommandAdapter
where
    Context: Send,
{
    async fn execute(
        &self,
        _context: &mut Context,
        invocation: CommandInvocation,
    ) -> Result<Value, CommandExecutionError> {
        let result = async {
            let connection = self
                .endpoint
                .connect(&CancellationToken::default())
                .await
                .map_err(|error| CommandExecutionError::new(error.code, error.message))?;
            connection
                .enforcer()
                .check_command(&self.name)
                .map_err(|error| {
                    CommandExecutionError::new("capability_violation", error.to_string())
                })?;
            connection
                .client()
                .call_command(
                    CommandExecuteParams {
                        lifetime: rw_plugin_protocol::OperationLifetime::new(
                            rw_operation_contract::MAX_OPERATION_DURATION_MS,
                            rw_operation_contract::MAX_OPERATION_DURATION_MS,
                        )
                        .map_err(|error| {
                            CommandExecutionError::new("invalid_lifetime", error.to_string())
                        })?,
                        invocation_id: invocation.origin().cloned(),
                        name: self.name.clone(),
                        arguments: invocation.arguments().to_owned(),
                    },
                    &CancellationToken::default(),
                )
                .await
                .map_err(|error| CommandExecutionError::new(error.code, error.message))
        }
        .await;
        self.endpoint
            .settle_effects()
            .await
            .map_err(|error| CommandExecutionError::new("effects_unsettled", error.to_string()))?;
        result
    }
}

pub struct RpcProviderAdapter {
    name: String,
    alias_prefix: String,
    capabilities: Capabilities,
    endpoint: Arc<dyn PluginEndpoint>,
    model_catalog: bool,
    catalog_cache: StdRwLock<RpcProviderCatalogCache>,
}

#[derive(Clone, Debug, Default)]
struct RpcProviderCatalogCache {
    catalog: Option<DiscoveredProviderCatalog>,
    aggregate_capabilities: Option<Capabilities>,
    single_model_metadata: Option<ProviderModelMetadata>,
    metadata_by_model: BTreeMap<String, ProviderModelMetadata>,
}

impl RpcProviderAdapter {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        alias_prefix: impl Into<String>,
        capabilities: Capabilities,
        endpoint: Arc<dyn PluginEndpoint>,
    ) -> Self {
        Self {
            name: name.into(),
            alias_prefix: alias_prefix.into(),
            capabilities,
            endpoint,
            model_catalog: false,
            catalog_cache: StdRwLock::new(RpcProviderCatalogCache::default()),
        }
    }

    /// Enables protocol-3 model discovery for an approval-fingerprinted provider declaration.
    #[must_use]
    pub fn with_model_catalog(mut self) -> Self {
        self.model_catalog = true;
        self
    }

    fn cached_capabilities(&self) -> Option<Capabilities> {
        self.catalog_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .aggregate_capabilities
            .clone()
    }

    #[allow(
        clippy::too_many_lines,
        reason = "catalog validation keeps the complete untrusted wire boundary visible"
    )]
    fn parse_catalog(&self, value: Value) -> Result<RpcProviderCatalogCache, ProviderError> {
        let response: ProviderModelsResponse = serde_json::from_value(value).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "plugin returned an invalid provider model catalog",
            )
        })?;
        if response.models.len() > rw_plugin_protocol::MAX_CAPABILITIES_PER_KIND {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "plugin provider model catalog exceeds the entry limit",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut models = Vec::with_capacity(response.models.len());
        let mut metadata = Vec::with_capacity(response.models.len());
        let mut metadata_by_model = BTreeMap::new();
        for model in response.models {
            if model.id.is_empty()
                || model.id.len() > MAX_NAME_BYTES
                || model.id.chars().any(char::is_control)
                || !ids.insert(model.id.clone())
            {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "plugin provider model id is invalid or duplicated",
                ));
            }
            if model.display_name.as_ref().is_some_and(|name| {
                name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control)
            }) {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "plugin provider model display name is invalid",
                ));
            }
            let max_context_tokens = model
                .max_context_tokens
                .map(|limit| limit.clamp(1, MAX_PLUGIN_MODEL_TOKENS));
            let max_output_tokens = model
                .max_output_tokens
                .map(|limit| limit.clamp(1, MAX_PLUGIN_MODEL_TOKENS));
            let capabilities = Capabilities {
                tool_calling: model.capabilities.tool_calling,
                vision: model.capabilities.vision,
                thinking: model.capabilities.thinking,
                cache_breakpoints: match model.capabilities.cache_breakpoints {
                    ProviderCacheBreakpoints::None => CacheBreakpointSupport::None,
                    ProviderCacheBreakpoints::Explicit => CacheBreakpointSupport::Explicit,
                    ProviderCacheBreakpoints::Automatic => CacheBreakpointSupport::Automatic,
                },
                max_context_tokens,
                max_output_tokens,
                wire_mode: WireMode::NormalizedReplay,
            };
            let pricing = model.pricing.map(|pricing| ModelPricing {
                display_name: model
                    .display_name
                    .clone()
                    .unwrap_or_else(|| model.id.clone()),
                max_context_tokens,
                max_output_tokens,
                supports_tools: capabilities.tool_calling,
                supports_thinking: capabilities.thinking,
                supports_vision: capabilities.vision,
                reasoning_efforts: Vec::new(),
                input_per_million_micros_usd: pricing
                    .input_per_million_micros_usd
                    .min(MAX_PLUGIN_PRICE_MICROS_USD),
                output_per_million_micros_usd: pricing
                    .output_per_million_micros_usd
                    .min(MAX_PLUGIN_PRICE_MICROS_USD),
                cache_read_per_million_micros_usd: pricing
                    .cache_read_per_million_micros_usd
                    .map(|price| price.min(MAX_PLUGIN_PRICE_MICROS_USD)),
                cache_write_per_million_micros_usd: pricing
                    .cache_write_per_million_micros_usd
                    .map(|price| price.min(MAX_PLUGIN_PRICE_MICROS_USD)),
                reasoning_per_million_micros_usd: pricing
                    .reasoning_per_million_micros_usd
                    .map(|price| price.min(MAX_PLUGIN_PRICE_MICROS_USD)),
            });
            let model_metadata = ProviderModelMetadata {
                capabilities: capabilities.clone(),
                accounting: if pricing.is_some() {
                    UsageAccounting::ApiDollars
                } else {
                    UsageAccounting::UnpricedApi
                },
                pricing: pricing.clone(),
            };
            metadata_by_model.insert(model.id.clone(), model_metadata.clone());
            metadata.push(model_metadata);
            models.push(DiscoveredModel {
                id: model.id,
                display_name: model.display_name,
                description: None,
                capabilities: Some(capabilities),
                pricing,
            });
        }
        let aggregate_capabilities = aggregate_plugin_capabilities(&metadata, &self.capabilities);
        Ok(RpcProviderCatalogCache {
            catalog: Some(DiscoveredProviderCatalog {
                provider: self.alias_prefix.trim_end_matches('/').to_owned(),
                models,
            }),
            aggregate_capabilities: Some(aggregate_capabilities),
            single_model_metadata: (metadata.len() == 1).then(|| metadata.remove(0)),
            metadata_by_model,
        })
    }
}

fn aggregate_plugin_capabilities(
    metadata: &[ProviderModelMetadata],
    fallback: &Capabilities,
) -> Capabilities {
    let Some(first) = metadata.first() else {
        return fallback.clone();
    };
    Capabilities {
        tool_calling: metadata.iter().all(|entry| entry.capabilities.tool_calling),
        vision: metadata.iter().all(|entry| entry.capabilities.vision),
        thinking: metadata.iter().all(|entry| entry.capabilities.thinking),
        cache_breakpoints: if metadata.iter().all(|entry| {
            entry.capabilities.cache_breakpoints == first.capabilities.cache_breakpoints
        }) {
            first.capabilities.cache_breakpoints
        } else {
            CacheBreakpointSupport::None
        },
        max_context_tokens: common_plugin_limit(metadata, |entry| {
            entry.capabilities.max_context_tokens
        }),
        max_output_tokens: common_plugin_limit(metadata, |entry| {
            entry.capabilities.max_output_tokens
        }),
        wire_mode: WireMode::NormalizedReplay,
    }
}

fn common_plugin_limit(
    metadata: &[ProviderModelMetadata],
    get: impl Fn(&ProviderModelMetadata) -> Option<u64>,
) -> Option<u64> {
    metadata.iter().try_fold(u64::MAX, |minimum, entry| {
        get(entry).map(|limit| minimum.min(limit))
    })
}

#[async_trait]
impl Provider for RpcProviderAdapter {
    async fn continuation_provenance(
        &self,
    ) -> Result<Option<rw_providers::ContinuationProvenance>, ProviderError> {
        let connection = self
            .endpoint
            .connect(&CancellationToken::default())
            .await
            .map_err(|error| ProviderError::new(ProviderErrorKind::Protocol, error.to_string()))?;
        Ok(Some(connection.continuation_provenance().clone()))
    }

    async fn settle_effects(&self) -> std::result::Result<(), rw_providers::ProviderError> {
        self.endpoint.settle_effects().await.map_err(|error| {
            ProviderError::new(ProviderErrorKind::EffectsUnsettled, error.to_string())
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> Capabilities {
        self.cached_capabilities()
            .unwrap_or_else(|| self.capabilities.clone())
    }
    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        if let Some(metadata) = self.cached_model_metadata() {
            return Ok(Some(metadata));
        }
        let _ = self.discover_models().await?;
        Ok(self.cached_model_metadata())
    }
    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        self.catalog_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .single_model_metadata
            .clone()
    }
    fn cached_model_metadata_for(&self, model: &str) -> Option<ProviderModelMetadata> {
        self.catalog_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .metadata_by_model
            .get(model)
            .cloned()
    }
    async fn discover_models(&self) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        if !self.model_catalog {
            return Ok(None);
        }
        let connection = self
            .endpoint
            .connect(&CancellationToken::default())
            .await
            .map_err(|error| provider_rpc_error(&error))?;
        connection
            .enforcer()
            .check_provider(&format!("{}catalog", self.alias_prefix))
            .map_err(|error| {
                ProviderError::new(ProviderErrorKind::Unsupported, error.to_string())
            })?;
        let value = connection
            .client()
            .request(
                METHOD_PROVIDER_MODELS,
                serde_json::to_value(ProviderModelsParams {
                    alias_prefix: self.alias_prefix.clone(),
                })
                .map_err(|error| {
                    ProviderError::new(ProviderErrorKind::Protocol, error.to_string())
                })?,
            )
            .await
            .map_err(|error| ProviderError::new(ProviderErrorKind::Protocol, error.to_string()))?;
        let cache = self.parse_catalog(value)?;
        let catalog = cache.catalog.clone();
        *self
            .catalog_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cache;
        Ok(catalog)
    }
    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        let alias = format!("{}{}", self.alias_prefix, request.model);
        let connection = self
            .endpoint
            .connect(&CancellationToken::default())
            .await
            .map_err(|error| provider_rpc_error(&error))?;
        connection
            .enforcer()
            .check_provider(&alias)
            .map_err(|error| {
                ProviderError::new(ProviderErrorKind::Unsupported, error.to_string())
            })?;
        let events = connection
            .client()
            .provider_stream(
                serde_json::to_value(ProviderCompleteParams {
                    alias,
                    request: serde_json::to_value(request).map_err(|error| {
                        ProviderError::new(ProviderErrorKind::Protocol, error.to_string())
                    })?,
                })
                .map_err(|error| {
                    ProviderError::new(ProviderErrorKind::Protocol, error.to_string())
                })?,
            )
            .await
            .map_err(|error| provider_rpc_error(&error))?;
        Ok(Box::pin(events.map(|event| {
            let value = event.map_err(|error| provider_rpc_error(&error))?;
            serde_json::from_value(value).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "plugin returned an invalid provider event",
                )
            })
        })))
    }
}

fn provider_rpc_error(error: &PluginRpcError) -> ProviderError {
    let kind = match error.code.as_str() {
        "provider_http_authentication" | "authentication" => ProviderErrorKind::Authentication,
        "provider_http_rate_limited" => ProviderErrorKind::RateLimited,
        "provider_http_timeout" | "timeout" => ProviderErrorKind::Timeout,
        "effects_unsettled" => ProviderErrorKind::EffectsUnsettled,
        "provider_http_server" => ProviderErrorKind::Server,
        "provider_http_network" => ProviderErrorKind::Network,
        "provider_http_network_disabled" => ProviderErrorKind::NetworkDisabled,
        "provider_http_cancelled" | "cancelled" => ProviderErrorKind::Cancelled,
        "provider_http_invalid_request" | "invalid_request" | "domain_denied" => {
            ProviderErrorKind::InvalidRequest
        }
        _ => ProviderErrorKind::Protocol,
    };
    ProviderError::new(kind, error.to_string())
}

pub struct PluginEventRouter {
    endpoint: Arc<dyn PluginEndpoint>,
}

impl PluginEventRouter {
    #[must_use]
    pub fn new(endpoint: Arc<dyn PluginEndpoint>) -> Self {
        Self { endpoint }
    }
    /// Calls one subscribed event handler under the ordinary fixed RPC deadline.
    ///
    /// # Errors
    /// Rejects invalid notices, undeclared subscriptions and unsettled RPC failures.
    pub async fn deliver(
        &self,
        notice: ExtensionEventNotice,
        cancellation: &CancellationToken,
    ) -> Result<ExtensionEventOutcome, PluginRpcError> {
        notice
            .validate()
            .map_err(|message| rpc_error("invalid_event", message))?;
        let connection = self.endpoint.connect(cancellation).await?;
        connection
            .enforcer()
            .check_event(notice.event)
            .map_err(|error| rpc_error("capability_violation", &error.to_string()))?;
        let response = connection
            .client()
            .request_cancellable(
                METHOD_EVENT_PUBLISH,
                serde_json::to_value(notice)
                    .map_err(|_| rpc_error("invalid_event", "event encoding failed"))?,
                cancellation,
            )
            .await?;
        serde_json::from_value(response)
            .map_err(|_| rpc_error("invalid_event_outcome", "invalid event outcome"))
    }

    /// # Errors
    /// Returns failed native/process proof after cancellation or caller drop.
    pub async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        self.endpoint.settle_effects().await
    }
}
