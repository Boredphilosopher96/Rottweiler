#![cfg(test)]

use crate::engine::AgentLoopError;
use crate::engine::model::ModelContextMetadata;
use crate::engine::model::ModelDriver;
use crate::engine::tests::fixtures::support::stop_script;
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream;
use rw_providers::BoxEventStream;
use rw_providers::CacheBreakpointSupport;
use rw_providers::Capabilities;
use rw_providers::FinishReason;
use rw_providers::Provider;
use rw_providers::ProviderError;
use rw_providers::ProviderErrorKind;
use rw_providers::ProviderEvent;
use rw_providers::ProviderRequest;
use rw_providers::ProviderRouter;
use rw_providers::RetryPolicy;
use rw_providers::TokenUsage;
use rw_types::Block;
use rw_types::Cost;
use rw_types::Role;
use rw_types::config::BudgetConfig;
use rw_types::config::CompactionConfig;
use rw_types::config::ThinkingLevel;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;

pub(in crate::engine::tests) type ProviderScript = Vec<Result<ProviderEvent, ProviderError>>;

#[derive(Default)]
pub(in crate::engine::tests) struct ScriptedModel {
    pub(in crate::engine::tests) scripts: Mutex<VecDeque<ProviderScript>>,
    pub(in crate::engine::tests) requests: Mutex<Vec<ProviderRequest>>,
    pub(in crate::engine::tests) aliases: Mutex<Vec<String>>,
    pub(in crate::engine::tests) title_enabled: AtomicBool,
}

pub(in crate::engine::tests) struct AliasVisionModel;

#[derive(Default)]
pub(in crate::engine::tests) struct DeferredVisionModel {
    pub(in crate::engine::tests) prepared: AtomicBool,
}

#[async_trait]
impl ModelDriver for DeferredVisionModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::MessageStart {
                model: "vision/model".to_owned(),
            }),
            Ok(ProviderEvent::TextDelta {
                text: "image received".to_owned(),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }

    async fn prepare_model(&self, _alias: &str) -> Result<(), AgentLoopError> {
        self.prepared.store(true, Ordering::Release);
        Ok(())
    }

    fn supports_vision(&self, _alias: &str) -> bool {
        self.prepared.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl ModelDriver for AliasVisionModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Err(AgentLoopError::Provider(
            "alias fixture does not make provider calls".to_owned(),
        ))
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        matches!(alias, "fast" | "slow") || alias.contains('/')
    }

    fn thinking_for_model(&self, model: &str, fallback: ThinkingLevel) -> ThinkingLevel {
        if model == "slow" {
            ThinkingLevel::High
        } else {
            fallback
        }
    }

    fn has_provider_for_alias(&self, alias: &str, provider: &str) -> bool {
        alias == "slow" && provider == "offline"
    }

    fn supports_vision(&self, alias: &str) -> bool {
        alias == "slow"
    }
}

impl ScriptedModel {
    pub(in crate::engine::tests) fn new(scripts: impl IntoIterator<Item = ProviderScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            aliases: Mutex::new(Vec::new()),
            title_enabled: AtomicBool::new(false),
        }
    }

    pub(in crate::engine::tests) fn with_title_alias(self) -> Self {
        self.title_enabled.store(true, Ordering::Release);
        self
    }

    pub(in crate::engine::tests) fn request_count(&self) -> usize {
        self.requests.lock().expect("request lock").len()
    }

    pub(in crate::engine::tests) fn aliases(&self) -> Vec<String> {
        self.aliases.lock().expect("alias lock").clone()
    }
}

#[async_trait::async_trait]
impl ModelDriver for ScriptedModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.aliases
            .lock()
            .expect("alias lock")
            .push(alias.to_owned());
        self.requests.lock().expect("request lock").push(request);
        let events = self
            .scripts
            .lock()
            .expect("script lock")
            .pop_front()
            .ok_or_else(|| AgentLoopError::Provider("missing fixture script".to_owned()))?;
        Ok(Box::pin(stream::iter(events)))
    }

    fn title_model_alias(&self) -> Option<String> {
        self.title_enabled
            .load(Ordering::Acquire)
            .then(|| "fast".to_owned())
    }
}

pub(in crate::engine::tests) struct M3Model {
    pub(in crate::engine::tests) scripts: Mutex<VecDeque<ProviderScript>>,
    pub(in crate::engine::tests) requests: Mutex<Vec<ProviderRequest>>,
    pub(in crate::engine::tests) operations: Mutex<Vec<String>>,
    pub(in crate::engine::tests) metadata: ModelContextMetadata,
    pub(in crate::engine::tests) compaction: CompactionConfig,
    pub(in crate::engine::tests) budget: BudgetConfig,
    pub(in crate::engine::tests) cost_override: Option<Cost>,
}

impl M3Model {
    pub(in crate::engine::tests) fn new(scripts: impl IntoIterator<Item = ProviderScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            operations: Mutex::new(Vec::new()),
            metadata: ModelContextMetadata::default(),
            compaction: CompactionConfig::default(),
            budget: BudgetConfig::default(),
            cost_override: None,
        }
    }

    pub(in crate::engine::tests) fn requests(&self) -> Vec<ProviderRequest> {
        self.requests.lock().expect("request lock").clone()
    }

    pub(in crate::engine::tests) fn operations(&self) -> Vec<String> {
        self.operations.lock().expect("operation lock").clone()
    }
}

#[async_trait]
impl ModelDriver for M3Model {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.operations
            .lock()
            .expect("operation lock")
            .push(format!("stream:{alias}"));
        self.requests.lock().expect("request lock").push(request);
        let script = self
            .scripts
            .lock()
            .expect("script lock")
            .pop_front()
            .ok_or_else(|| AgentLoopError::Provider("missing M3 script".to_owned()))?;
        Ok(Box::pin(stream::iter(script)))
    }

    async fn prepare_model(&self, alias: &str) -> Result<(), AgentLoopError> {
        self.operations
            .lock()
            .expect("operation lock")
            .push(format!("prepare:{alias}"));
        Ok(())
    }

    fn context_metadata(&self, _alias: &str) -> ModelContextMetadata {
        self.metadata
    }

    fn compaction_config(&self) -> CompactionConfig {
        self.compaction.clone()
    }

    fn budget_config(&self) -> BudgetConfig {
        self.budget.clone()
    }

    fn cost(&self, _alias: &str, usage: TokenUsage) -> Cost {
        if let Some(cost) = &self.cost_override {
            return cost.clone();
        }
        Cost::Monetary {
            amount_micros: usage.output_tokens,
            currency: "USD".to_owned(),
        }
    }
}

pub(in crate::engine::tests) struct ReplaySourceProvider {
    pub(in crate::engine::tests) scripts: Mutex<VecDeque<ProviderScript>>,
}

#[async_trait]
impl Provider for ReplaySourceProvider {
    async fn settle_effects(&self) -> Result<(), rw_providers::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "context-replay"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: false,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::Explicit,
            max_context_tokens: Some(2_000),
            max_output_tokens: Some(256),
            wire_mode: rw_providers::WireMode::NormalizedReplay,
        }
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        let script = self
            .scripts
            .lock()
            .expect("replay source scripts")
            .pop_front()
            .ok_or_else(|| {
                ProviderError::new(ProviderErrorKind::ReplayMiss, "missing source script")
            })?;
        Ok(Box::pin(stream::iter(script)))
    }
}

pub(in crate::engine::tests) struct ReplayHarnessModel {
    pub(in crate::engine::tests) router: ProviderRouter,
}

impl ReplayHarnessModel {
    pub(in crate::engine::tests) fn new(provider: Arc<dyn Provider>) -> Self {
        let router = ProviderRouter::new(
            BTreeMap::from([("fast".to_owned(), vec!["context-replay/model".to_owned()])]),
            [provider],
            RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
                jitter_fraction: 0.0,
            },
        )
        .expect("replay router");
        Self { router }
    }
}

#[async_trait::async_trait]
impl ModelDriver for ReplayHarnessModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        self.router
            .settle_effects()
            .await
            .map_err(|error| crate::AgentLoopError::EffectsUnsettled(error.to_string()))
    }

    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.router
            .stream_alias(
                alias,
                request,
                Arc::new(crate::provider_admission::gate::InvocationGate {
                    invocation,
                    metadata: BTreeMap::new(),
                }),
            )
            .map_err(|error| AgentLoopError::Provider(error.to_string()))
    }

    fn context_metadata(&self, _alias: &str) -> ModelContextMetadata {
        ModelContextMetadata {
            max_context_tokens: Some(2_000),
            max_output_tokens: Some(256),
            cache_breakpoints: Some(CacheBreakpointSupport::Explicit),
        }
    }

    fn budget_config(&self) -> BudgetConfig {
        BudgetConfig {
            session_cost_cap_micros_usd: Some(100),
            ..BudgetConfig::default()
        }
    }

    fn cost(&self, _alias: &str, usage: TokenUsage) -> Cost {
        Cost::Monetary {
            amount_micros: usage.output_tokens,
            currency: "USD".to_owned(),
        }
    }
}

pub(in crate::engine::tests) struct RoutedCostModel {
    pub(in crate::engine::tests) route: &'static str,
    pub(in crate::engine::tests) requests: AtomicUsize,
    pub(in crate::engine::tests) budget: BudgetConfig,
}

impl RoutedCostModel {
    pub(in crate::engine::tests) fn new(route: &'static str) -> Self {
        Self {
            route,
            requests: AtomicUsize::new(0),
            budget: BudgetConfig {
                session_cost_cap_micros_usd: Some(50),
                ..BudgetConfig::default()
            },
        }
    }
}

#[async_trait::async_trait]
impl ModelDriver for RoutedCostModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::iter([
            Ok(ProviderEvent::RouteSelected {
                route: self.route.to_owned(),
            }),
            Ok(ProviderEvent::MessageStart {
                model: "shared-model-id".to_owned(),
            }),
            Ok(ProviderEvent::TextDelta {
                text: "done".to_owned(),
            }),
            Ok(ProviderEvent::Usage {
                usage: TokenUsage {
                    output_tokens: 1,
                    ..TokenUsage::default()
                },
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }

    fn budget_config(&self) -> BudgetConfig {
        self.budget.clone()
    }

    fn cost_for_route(
        &self,
        _alias: &str,
        route: Option<&str>,
        _reported_model: Option<&str>,
        _usage: TokenUsage,
    ) -> Cost {
        let amount_micros = match route {
            Some("__model_cheap") => 10,
            Some("__model_expensive") => 100,
            _ => {
                return Cost::Unavailable {
                    reason: "unknown route".to_owned(),
                };
            }
        };
        Cost::Monetary {
            amount_micros,
            currency: "USD".to_owned(),
        }
    }

    fn qualified_model_for_route(
        &self,
        _alias: &str,
        route: Option<&str>,
        _reported_model: Option<&str>,
    ) -> Option<String> {
        match route {
            Some("__model_cheap") => Some("cheap/shared-model-id".to_owned()),
            Some("__model_expensive") => Some("expensive/shared-model-id".to_owned()),
            _ => None,
        }
    }
}

pub(in crate::engine::tests) struct DelayedSummaryModel;

#[async_trait::async_trait]
impl ModelDriver for DelayedSummaryModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Ok(Box::pin(
            stream::iter([
                Ok(ProviderEvent::MessageStart {
                    model: "fixture-model".to_owned(),
                }),
                Ok(ProviderEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 11,
                        output_tokens: 7,
                        ..TokenUsage::default()
                    },
                }),
            ])
            .chain(stream::once(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(ProviderEvent::TextDelta {
                    text: "summary".to_owned(),
                })
            })),
        ))
    }
}

pub(in crate::engine::tests) struct PendingModel;

#[async_trait::async_trait]
impl ModelDriver for PendingModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Ok(Box::pin(
            stream::iter([Ok(ProviderEvent::MessageStart {
                model: "fixture-model".to_owned(),
            })])
            .chain(stream::pending::<Result<ProviderEvent, ProviderError>>()),
        ))
    }
}

pub(in crate::engine::tests) struct GatedCompactionModel {
    pub(in crate::engine::tests) calls: AtomicUsize,
    pub(in crate::engine::tests) started: Arc<Notify>,
    pub(in crate::engine::tests) release: Arc<Notify>,
}

#[async_trait::async_trait]
impl ModelDriver for GatedCompactionModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let started = Arc::clone(&self.started);
            let release = Arc::clone(&self.release);
            return Ok(Box::pin(
                    stream::once(async move {
                        started.notify_one();
                        release.notified().await;
                        Ok(ProviderEvent::TextDelta {
                            text: "## Goal\ncontinue\n\n## Instructions\n\n## Discoveries\n\n## Accomplished\n\n## Relevant files & directories\n".to_owned(),
                        })
                    })
                    .chain(stream::iter([Ok(ProviderEvent::Finished {
                        reason: FinishReason::Stop,
                    })])),
                ));
        }
        Ok(Box::pin(stream::iter(stop_script("queued answer", &[]))))
    }
}

pub(in crate::engine::tests) struct DelayedFinishModel {
    pub(in crate::engine::tests) delay: Duration,
}

#[async_trait::async_trait]
impl ModelDriver for DelayedFinishModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        let delay = self.delay;
        Ok(Box::pin(
            stream::iter([
                Ok(ProviderEvent::MessageStart {
                    model: "fixture-model".to_owned(),
                }),
                Ok(ProviderEvent::TextDelta {
                    text: "visible promptly".to_owned(),
                }),
            ])
            .chain(stream::once(async move {
                tokio::time::sleep(delay).await;
                Ok(ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                })
            })),
        ))
    }
}

pub(in crate::engine::tests) struct ContinuousDeltaModel {
    pub(in crate::engine::tests) count: usize,
    pub(in crate::engine::tests) delay: Duration,
}

#[async_trait::async_trait]
impl ModelDriver for ContinuousDeltaModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        let count = self.count;
        let delay = self.delay;
        let deltas = stream::unfold(0_usize, move |index| async move {
            if index > count {
                return None;
            }
            tokio::time::sleep(delay).await;
            let event = if index == count {
                ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                }
            } else {
                ProviderEvent::TextDelta {
                    text: "x".to_owned(),
                }
            };
            Some((Ok(event), index.saturating_add(1)))
        });
        Ok(Box::pin(
            stream::iter([Ok(ProviderEvent::MessageStart {
                model: "fixture-model".to_owned(),
            })])
            .chain(deltas),
        ))
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct InstructionModel {
    pub(in crate::engine::tests) observed: AtomicBool,
}

#[async_trait::async_trait]
impl ModelDriver for InstructionModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        let steered = request.turns.iter().any(|turn| {
            turn.role == Role::System
                && turn.blocks.iter().any(
                    |block| matches!(block, Block::Text { text } if text.contains("reply kennel")),
                )
        });
        self.observed.store(steered, Ordering::SeqCst);
        if !steered {
            return Err(AgentLoopError::Provider(
                "fixture root instruction was absent".to_owned(),
            ));
        }
        Ok(Box::pin(stream::iter(stop_script("kennel", &[]))))
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct GatedCleanupProvider {
    pub(in crate::engine::tests) invoked: Notify,
    pub(in crate::engine::tests) cleanup: Notify,
    pub(in crate::engine::tests) release: Notify,
    pub(in crate::engine::tests) settled: AtomicBool,
}

#[async_trait]
impl rw_providers::Provider for GatedCleanupProvider {
    fn name(&self) -> &'static str {
        "gated"
    }
    fn capabilities(&self) -> rw_providers::Capabilities {
        rw_providers::Capabilities {
            tool_calling: false,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: rw_providers::WireMode::NormalizedReplay,
        }
    }
    async fn stream(
        &self,
        _request: ProviderRequest,
    ) -> Result<BoxEventStream, rw_providers::ProviderError> {
        self.invoked.notify_one();
        std::future::pending().await
    }
    async fn settle_effects(&self) -> Result<(), rw_providers::ProviderError> {
        self.cleanup.notify_one();
        self.release.notified().await;
        self.settled.store(true, Ordering::SeqCst);
        Ok(())
    }
}

pub(in crate::engine::tests) struct CleanupModel(
    pub(in crate::engine::tests) rw_providers::ProviderRouter,
);

#[async_trait]
impl ModelDriver for CleanupModel {
    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.0
            .stream_alias(
                alias,
                request,
                Arc::new(crate::provider_admission::gate::InvocationGate {
                    invocation,
                    metadata: BTreeMap::new(),
                }),
            )
            .map_err(|error| AgentLoopError::Provider(error.to_string()))
    }
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        self.0
            .settle_effects()
            .await
            .map_err(|error| AgentLoopError::EffectsUnsettled(error.to_string()))
    }
}
