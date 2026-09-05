use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_stream::stream;
use futures_util::StreamExt;
use thiserror::Error;
use tokio::time::Instant;

use crate::{
    BoxEventStream, Clock, Delay, JitterSource, ProductionJitter, Provider, ProviderAttemptGate,
    ProviderError, ProviderEvent, ProviderRequest, RetryPolicy, TokioClock, TokioDelay,
};

const DEFAULT_FAILOVER_COOLDOWN: Duration = Duration::from_secs(30);

/// A provider-qualified candidate model.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ModelCandidate {
    /// Provider registry key.
    pub provider: String,
    /// Provider-local model id.
    pub model: String,
}

impl ModelCandidate {
    /// Parses `provider/model`, retaining all later slashes in the model id.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not provider-qualified.
    pub fn parse(value: &str) -> Result<Self, RouterError> {
        let Some((provider, model)) = value.split_once('/') else {
            return Err(RouterError::InvalidCandidate(value.to_owned()));
        };
        if provider.is_empty() || model.is_empty() {
            return Err(RouterError::InvalidCandidate(value.to_owned()));
        }
        Ok(Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
        })
    }
}

/// Router configuration or dispatch error.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RouterError {
    /// Active or unsettled operations exhausted bounded admission.
    #[error("provider operation admission failed: {0}")]
    OperationAdmission(String),
    /// Alias is missing or has no candidates.
    #[error("model alias '{0}' is not configured; add an ordered provider/model chain")]
    AliasNotConfigured(String),
    /// Candidate is not provider-qualified.
    #[error("invalid model candidate '{0}'; expected provider/model")]
    InvalidCandidate(String),
    /// Candidate refers to an unregistered provider.
    #[error("model candidate references unregistered provider '{0}'")]
    ProviderNotRegistered(String),
    /// Explicit provider route is not present in the selected alias.
    #[error("model alias '{alias}' has no route through provider '{provider}'")]
    ProviderNotAvailable { alias: String, provider: String },
}

/// Provider-blind alias router with ordered failover chains.
pub struct ProviderRouter {
    operations: crate::settlement::ProviderOperations,
    aliases: BTreeMap<String, Vec<ModelCandidate>>,
    providers: BTreeMap<String, Arc<dyn Provider>>,
    retry: RetryPolicy,
    delay: Arc<dyn Delay>,
    jitter: Arc<dyn JitterSource>,
    clock: Arc<dyn Clock>,
    failover_cooldown: Duration,
    cooldowns: Arc<Mutex<BTreeMap<ModelCandidate, Instant>>>,
}

impl std::fmt::Debug for ProviderRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRouter")
            .field("aliases", &self.aliases)
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl ProviderRouter {
    /// Retains the exact invoked provider through asynchronous local settlement.
    ///
    /// # Errors
    /// Returns an admission error while the bounded operation registry is full.
    pub fn stream_provider(
        &self,
        candidate: ModelCandidate,
        provider: Arc<dyn Provider>,
        request: ProviderRequest,
        gate: Arc<dyn ProviderAttemptGate>,
    ) -> Result<BoxEventStream, RouterError> {
        self.operations
            .stream(
                provider,
                request,
                crate::settlement::AttemptEntry {
                    candidate,
                    gate,
                    number: 0,
                },
            )
            .map_err(|error| RouterError::OperationAdmission(error.to_string()))
    }

    /// Waits for abandoned invocation owners, including ones no longer in a catalog.
    ///
    /// # Errors
    /// Returns an error when native effects or durable accounting remain unproven.
    pub async fn settle_effects(&self) -> Result<(), ProviderError> {
        self.operations.settle().await
    }

    /// Creates a router from provider-qualified alias chains.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed candidates or missing providers.
    pub fn new(
        aliases: BTreeMap<String, Vec<String>>,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
        retry: RetryPolicy,
    ) -> Result<Self, RouterError> {
        let providers = providers
            .into_iter()
            .map(|provider| (provider.name().to_owned(), provider));
        Self::with_registry_and_delay(aliases, providers, retry, Arc::new(TokioDelay))
    }

    /// Creates a router from an explicit registry-key/provider mapping.
    ///
    /// This boundary lets a composition root register multiple model-bound
    /// views of one logical endpoint without changing the provider identity
    /// used by recording and diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed candidates or missing registry keys.
    pub fn with_registry(
        aliases: BTreeMap<String, Vec<String>>,
        providers: impl IntoIterator<Item = (String, Arc<dyn Provider>)>,
        retry: RetryPolicy,
    ) -> Result<Self, RouterError> {
        Self::with_registry_and_delay(aliases, providers, retry, Arc::new(TokioDelay))
    }

    /// Creates a router with injected time for deterministic tests/replay.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed candidates or missing providers.
    pub fn with_delay(
        aliases: BTreeMap<String, Vec<String>>,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
        retry: RetryPolicy,
        delay: Arc<dyn Delay>,
    ) -> Result<Self, RouterError> {
        let providers = providers
            .into_iter()
            .map(|provider| (provider.name().to_owned(), provider));
        Self::with_registry_and_delay(aliases, providers, retry, delay)
    }

    fn with_registry_and_delay(
        aliases: BTreeMap<String, Vec<String>>,
        providers: impl IntoIterator<Item = (String, Arc<dyn Provider>)>,
        retry: RetryPolicy,
        delay: Arc<dyn Delay>,
    ) -> Result<Self, RouterError> {
        Self::with_registry_and_timing(
            aliases,
            providers,
            retry,
            delay,
            Arc::new(ProductionJitter::default()),
            Arc::new(TokioClock),
            DEFAULT_FAILOVER_COOLDOWN,
        )
    }

    /// Creates a router with injected delay, jitter source, monotonic clock,
    /// and failover cooldown for deterministic health and retry tests.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed candidates or missing providers.
    pub fn with_timing(
        aliases: BTreeMap<String, Vec<String>>,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
        retry: RetryPolicy,
        delay: Arc<dyn Delay>,
        jitter: Arc<dyn JitterSource>,
        clock: Arc<dyn Clock>,
        failover_cooldown: Duration,
    ) -> Result<Self, RouterError> {
        let providers = providers
            .into_iter()
            .map(|provider| (provider.name().to_owned(), provider));
        Self::with_registry_and_timing(
            aliases,
            providers,
            retry,
            delay,
            jitter,
            clock,
            failover_cooldown,
        )
    }

    fn with_registry_and_timing(
        aliases: BTreeMap<String, Vec<String>>,
        providers: impl IntoIterator<Item = (String, Arc<dyn Provider>)>,
        retry: RetryPolicy,
        delay: Arc<dyn Delay>,
        jitter: Arc<dyn JitterSource>,
        clock: Arc<dyn Clock>,
        failover_cooldown: Duration,
    ) -> Result<Self, RouterError> {
        let aliases = aliases
            .into_iter()
            .map(|(alias, values)| {
                let candidates = values
                    .iter()
                    .map(|value| ModelCandidate::parse(value))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((alias, candidates))
            })
            .collect::<Result<BTreeMap<_, _>, RouterError>>()?;
        let providers = providers.into_iter().collect::<BTreeMap<_, _>>();
        for candidate in aliases.values().flatten() {
            if !providers.contains_key(&candidate.provider) {
                return Err(RouterError::ProviderNotRegistered(
                    candidate.provider.clone(),
                ));
            }
        }
        Ok(Self {
            operations: crate::settlement::ProviderOperations::default(),
            aliases,
            providers,
            retry,
            delay,
            jitter,
            clock,
            failover_cooldown,
            cooldowns: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Resolves an alias to its ordered candidate chain.
    ///
    /// # Errors
    ///
    /// Returns an error when the alias is absent or empty.
    pub fn resolve(&self, alias: &str) -> Result<&[ModelCandidate], RouterError> {
        self.aliases
            .get(alias)
            .filter(|candidates| !candidates.is_empty())
            .map(Vec::as_slice)
            .ok_or_else(|| RouterError::AliasNotConfigured(alias.to_owned()))
    }

    /// Streams through the first healthy candidate. Retries and failover are
    /// only permitted before semantic output, preventing duplicate assistant
    /// text or tool calls after a partial response.
    ///
    /// # Errors
    ///
    /// Returns an error when the alias is absent or empty.
    pub fn stream_alias(
        &self,
        alias: &str,
        request: ProviderRequest,
        gate: Arc<dyn ProviderAttemptGate>,
    ) -> Result<BoxEventStream, RouterError> {
        let candidates = self.resolve(alias)?.to_vec();
        self.stream_candidates(alias, candidates, request, gate)
    }

    /// Streams through an already validated subset of one alias's routes.
    /// This is used for an explicit provider selection, where cross-provider
    /// fallback would violate the user's route choice.
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied route subset is empty.
    #[allow(clippy::too_many_lines)]
    pub fn stream_candidates(
        &self,
        alias: &str,
        candidates: Vec<ModelCandidate>,
        request: ProviderRequest,
        gate: Arc<dyn ProviderAttemptGate>,
    ) -> Result<BoxEventStream, RouterError> {
        if candidates.is_empty() {
            return Err(RouterError::AliasNotConfigured(alias.to_owned()));
        }
        let providers = self.providers.clone();
        let operations = self.operations.clone();
        let retry = self.retry.clone();
        let delay = Arc::clone(&self.delay);
        let jitter = Arc::clone(&self.jitter);
        let clock = Arc::clone(&self.clock);
        let failover_cooldown = self.failover_cooldown;
        let cooldowns = Arc::clone(&self.cooldowns);
        let event_stream = stream! {
            let mut last_error = None;
            let mut next_attempt = Some(0_u32);
            'candidate: for candidate in candidates {
                if candidate_is_cooling(&cooldowns, &candidate, clock.now()) {
                    continue 'candidate;
                }
                let Some(provider) = providers.get(&candidate.provider).cloned() else {
                    yield Err(ProviderError::new(
                        crate::ProviderErrorKind::InvalidRequest,
                        format!("provider '{}' is no longer registered", candidate.provider),
                    ));
                    return;
                };
                let mut candidate_request = request.clone();
                candidate_request.model = candidate.model.clone();
                'attempt: for attempt in 0..retry.max_attempts.max(1) {
                    let Some(number) = next_attempt else {
                        yield Err(ProviderError::new(crate::ProviderErrorKind::InvalidRequest, "provider attempt identity exhausted"));
                        return;
                    };
                    next_attempt = number.checked_add(1);
                    let entry = crate::settlement::AttemptEntry { candidate: candidate.clone(), gate: gate.clone(), number };
                    let mut provider_stream = match operations.stream(Arc::clone(&provider), candidate_request.clone(), entry) {
                        Ok(provider_stream) => provider_stream,
                        Err(error) => {
                            let can_retry = error.is_retryable()
                                && attempt + 1 < retry.max_attempts.max(1);
                            last_error = Some(error.clone());
                            if can_retry {
                                wait_before_retry(&*delay, &*jitter, &retry, attempt, &error).await;
                                continue;
                            }
                            if error.is_retryable() {
                                mark_cooling(
                                    &cooldowns,
                                    candidate.clone(),
                                    clock.now(),
                                    failover_cooldown,
                                );
                                continue 'candidate;
                            }
                            yield Err(error);
                            return;
                        }
                    };
                    let mut pre_semantic = vec![ProviderEvent::RouteSelected {
                        route: candidate.provider.clone(),
                    }];
                    let mut semantic_emitted = false;
                    while let Some(item) = provider_stream.next().await {
                        match item {
                            Ok(event) => {
                                if is_semantic(&event) {
                                    for buffered in pre_semantic.drain(..) {
                                        yield Ok(buffered);
                                    }
                                    semantic_emitted = true;
                                    yield Ok(event);
                                } else if semantic_emitted {
                                    yield Ok(event);
                                } else {
                                    pre_semantic.push(event);
                                }
                            }
                            Err(error) => {
                                drop(provider_stream);
                                if let Err(unsettled) = operations.settle().await {
                                    yield Err(unsettled);
                                    return;
                                }
                                if semantic_emitted || !error.is_retryable() {
                                    yield Err(error);
                                    return;
                                }
                                last_error = Some(error.clone());
                                if attempt + 1 < retry.max_attempts.max(1) {
                                    wait_before_retry(&*delay, &*jitter, &retry, attempt, &error)
                                        .await;
                                    continue 'attempt;
                                }
                                mark_cooling(
                                    &cooldowns,
                                    candidate.clone(),
                                    clock.now(),
                                    failover_cooldown,
                                );
                                continue 'candidate;
                            }
                        }
                    }
                    for buffered in pre_semantic {
                        yield Ok(buffered);
                    }
                    return;
                }
            }
            yield Err(last_error.unwrap_or_else(|| ProviderError::new(
                crate::ProviderErrorKind::InvalidRequest,
                "all configured model candidates failed before producing output",
            )));
        };
        Ok(Box::pin(event_stream))
    }
}

async fn wait_before_retry(
    delay: &dyn Delay,
    jitter: &dyn JitterSource,
    retry: &RetryPolicy,
    attempt: usize,
    error: &ProviderError,
) {
    delay
        .sleep(retry.delay_for(attempt, error, jitter.sample_unit()))
        .await;
}

fn candidate_is_cooling(
    cooldowns: &Mutex<BTreeMap<ModelCandidate, Instant>>,
    candidate: &ModelCandidate,
    now: Instant,
) -> bool {
    let mut cooldowns = cooldowns
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match cooldowns.get(candidate).copied() {
        Some(deadline) if deadline > now => true,
        Some(_) => {
            cooldowns.remove(candidate);
            false
        }
        None => false,
    }
}

fn mark_cooling(
    cooldowns: &Mutex<BTreeMap<ModelCandidate, Instant>>,
    candidate: ModelCandidate,
    now: Instant,
    duration: Duration,
) {
    let mut cooldowns = cooldowns
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cooldowns.insert(candidate, now + duration);
}

const fn is_semantic(event: &ProviderEvent) -> bool {
    matches!(
        event,
        ProviderEvent::TextDelta { .. }
            | ProviderEvent::ThinkingDelta { .. }
            | ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallArgumentsDelta { .. }
            | ProviderEvent::ToolCallEnd { .. }
            | ProviderEvent::Citation { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use futures_util::StreamExt;
    use rw_types::config::ThinkingLevel;
    use tokio::time::Instant;

    use crate::{
        BoxEventStream, CacheBreakpointSupport, Capabilities, Clock, FinishReason, Provider,
        ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest, RetryPolicy,
        SeededJitter, TokioDelay, WireMode,
    };

    use super::ProviderRouter;

    struct FixtureProvider {
        name: String,
        calls: AtomicUsize,
        fail: AtomicBool,
    }

    struct PermanentFailureProvider {
        name: String,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FixtureProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            }
        }
        async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail.load(Ordering::Relaxed) {
                return Err(ProviderError::new(
                    ProviderErrorKind::Server,
                    "fixture down",
                ));
            }
            Ok(Box::pin(futures_util::stream::iter([
                Ok(ProviderEvent::MessageStart {
                    model: request.model,
                }),
                Ok(ProviderEvent::TextDelta {
                    text: "ok".to_owned(),
                }),
                Ok(ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    #[async_trait]
    impl Provider for PermanentFailureProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            }
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "permanent fixture failure",
            ))
        }
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: "ignored".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: crate::ToolChoice::Auto {},
            max_output_tokens: 100,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        }
    }

    #[tokio::test]
    async fn failed_provider_fails_over_before_semantic_output() {
        let dead = Arc::new(FixtureProvider {
            name: "dead".to_owned(),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(true),
        });
        let live = Arc::new(FixtureProvider {
            name: "live".to_owned(),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        });
        let providers: Vec<Arc<dyn Provider>> = vec![dead.clone(), live.clone()];
        let router = ProviderRouter::new(
            BTreeMap::from([(
                "fast".to_owned(),
                vec!["dead/a".to_owned(), "live/b".to_owned()],
            )]),
            providers,
            RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            },
        )
        .unwrap_or_else(|error| panic!("router must build: {error}"));
        let events = router
            .stream_alias("fast", request(), crate::attempt::fixture_gate())
            .unwrap_or_else(|error| panic!("alias must resolve: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(dead.calls.load(Ordering::Relaxed), 1);
        assert_eq!(live.calls.load(Ordering::Relaxed), 1);
        assert!(
            events.iter().any(
                |event| matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "ok")
            )
        );
    }

    #[tokio::test]
    async fn invalid_request_is_not_retried_or_failed_over() {
        let invalid = Arc::new(PermanentFailureProvider {
            name: "invalid".to_owned(),
            calls: AtomicUsize::new(0),
        });
        let fallback = Arc::new(FixtureProvider {
            name: "fallback".to_owned(),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        });
        let providers: Vec<Arc<dyn Provider>> = vec![invalid.clone(), fallback.clone()];
        let router = ProviderRouter::new(
            BTreeMap::from([(
                "fast".to_owned(),
                vec!["invalid/a".to_owned(), "fallback/b".to_owned()],
            )]),
            providers,
            RetryPolicy {
                max_attempts: 3,
                ..RetryPolicy::default()
            },
        )
        .unwrap_or_else(|error| panic!("router must build: {error}"));
        let events = router
            .stream_alias("fast", request(), crate::attempt::fixture_gate())
            .unwrap_or_else(|error| panic!("alias must resolve: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(invalid.calls.load(Ordering::Relaxed), 1);
        assert_eq!(fallback.calls.load(Ordering::Relaxed), 0);
        assert!(matches!(
            events.as_slice(),
            [Err(error)] if error.kind == ProviderErrorKind::InvalidRequest
        ));
    }

    #[derive(Debug)]
    struct FakeClock(Mutex<Instant>);

    impl FakeClock {
        fn advance(&self, duration: Duration) {
            let mut now = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *now += duration;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    #[tokio::test]
    async fn transient_failure_sticks_to_fallback_until_primary_cooldown_expires() {
        let primary = Arc::new(FixtureProvider {
            name: "primary".to_owned(),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(true),
        });
        let fallback = Arc::new(FixtureProvider {
            name: "fallback".to_owned(),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        });
        let clock = Arc::new(FakeClock(Mutex::new(Instant::now())));
        let providers: Vec<Arc<dyn Provider>> = vec![primary.clone(), fallback.clone()];
        let router = ProviderRouter::with_timing(
            BTreeMap::from([(
                "fast".to_owned(),
                vec!["primary/a".to_owned(), "fallback/b".to_owned()],
            )]),
            providers,
            RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            },
            Arc::new(TokioDelay),
            Arc::new(SeededJitter::new(7)),
            clock.clone(),
            Duration::from_secs(30),
        )
        .unwrap_or_else(|error| panic!("router must build: {error}"));

        let first = router
            .stream_alias("fast", request(), crate::attempt::fixture_gate())
            .unwrap_or_else(|error| panic!("alias must resolve: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert!(first.iter().any(Result::is_ok));
        assert_eq!(primary.calls.load(Ordering::Relaxed), 1);
        assert_eq!(fallback.calls.load(Ordering::Relaxed), 1);

        primary.fail.store(false, Ordering::Relaxed);
        let second = router
            .stream_alias("fast", request(), crate::attempt::fixture_gate())
            .unwrap_or_else(|error| panic!("alias must resolve: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert!(second.iter().any(Result::is_ok));
        assert_eq!(primary.calls.load(Ordering::Relaxed), 1);
        assert_eq!(fallback.calls.load(Ordering::Relaxed), 2);

        clock.advance(Duration::from_secs(30));
        let third = router
            .stream_alias("fast", request(), crate::attempt::fixture_gate())
            .unwrap_or_else(|error| panic!("alias must resolve: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert!(third.iter().any(Result::is_ok));
        assert_eq!(primary.calls.load(Ordering::Relaxed), 2);
        assert_eq!(fallback.calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn missing_alias_has_actionable_error() {
        let router = ProviderRouter::new(
            BTreeMap::new(),
            Vec::<Arc<dyn Provider>>::new(),
            RetryPolicy::default(),
        )
        .unwrap_or_else(|error| panic!("empty router is valid: {error}"));
        let Err(error) = router.resolve("fast") else {
            panic!("missing alias must fail");
        };
        assert!(error.to_string().contains("provider/model"));
    }
}
