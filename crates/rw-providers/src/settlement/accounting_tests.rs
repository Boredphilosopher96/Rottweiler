#![allow(clippy::unwrap_used)]

use super::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    invoked: AtomicUsize,
    entered: Mutex<Vec<u32>>,
    settled: Mutex<Vec<(u32, ProviderAttemptOutcome)>>,
    block_settlement: AtomicBool,
    settlement_entered: Notify,
    release: Notify,
    reject_after_first: AtomicBool,
    fail_provider: AtomicBool,
    fail_receipt: AtomicBool,
}
struct Gate(Arc<State>);
struct Attempt(Arc<State>, u32);
struct TestProvider(Arc<State>);

#[async_trait]
impl ProviderAttemptGate for Gate {
    async fn enter(
        &self,
        _: &ModelCandidate,
        _: &ProviderRequest,
        number: u32,
    ) -> Result<Box<dyn ProviderAttempt>, ProviderError> {
        self.0.entered.lock().unwrap().push(number);
        if number > 0 && self.0.reject_after_first.load(Ordering::Acquire) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "budget exhausted",
            ));
        }
        Ok(Box::new(Attempt(self.0.clone(), number)))
    }
}

#[async_trait]
impl ProviderAttempt for Attempt {
    async fn settle(self: Box<Self>, outcome: ProviderAttemptOutcome) -> Result<(), ProviderError> {
        self.0.settlement_entered.notify_one();
        if self.0.block_settlement.load(Ordering::Acquire) {
            self.0.release.notified().await;
        }
        if self.0.fail_receipt.load(Ordering::Acquire) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "receipt append failed",
            ));
        }
        self.0.settled.lock().unwrap().push((self.1, outcome));
        Ok(())
    }
}

#[async_trait]
impl Provider for TestProvider {
    fn name(&self) -> &'static str {
        "accounting-fixture"
    }
    fn capabilities(&self) -> crate::Capabilities {
        crate::Capabilities {
            tool_calling: false,
            vision: false,
            thinking: false,
            cache_breakpoints: crate::CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: crate::WireMode::NormalizedReplay,
        }
    }
    async fn stream(&self, _: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        self.0.invoked.fetch_add(1, Ordering::AcqRel);
        if self.0.fail_provider.load(Ordering::Acquire) {
            return Err(ProviderError::new(
                ProviderErrorKind::Server,
                "retryable provider failure",
            ));
        }
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::Usage {
                usage: crate::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..crate::TokenUsage::default()
                },
            }),
            Ok(ProviderEvent::Finished {
                reason: crate::FinishReason::Stop,
            }),
        ])))
    }
}

fn request() -> ProviderRequest {
    ProviderRequest {
        model: "model".into(),
        turns: Vec::new(),
        tools: Vec::new(),
        tool_choice: crate::ToolChoice::None,
        max_output_tokens: 10,
        temperature: None,
        thinking: rw_types::config::ThinkingLevel::Off,
        cache_hint: None,
    }
}
fn entry(state: &Arc<State>, number: u32) -> AttemptEntry {
    AttemptEntry {
        candidate: ModelCandidate {
            provider: "accounting-fixture".into(),
            model: "model".into(),
        },
        gate: Arc::new(Gate(state.clone())),
        number,
    }
}

#[tokio::test]
async fn unpolled_stream_never_enters_provider_or_accounting() {
    let operations = ProviderOperations::default();
    let state = Arc::new(State::default());
    drop(
        operations
            .stream(
                Arc::new(TestProvider(state.clone())),
                request(),
                entry(&state, 0),
            )
            .unwrap(),
    );
    operations.settle().await;
    assert_eq!(state.invoked.load(Ordering::Acquire), 0);
    assert!(state.entered.lock().unwrap().is_empty());
    assert!(state.settled.lock().unwrap().is_empty());
}

#[tokio::test]
async fn terminal_waits_for_receipt_and_drop_keeps_its_owner() {
    let operations = ProviderOperations::default();
    let state = Arc::new(State::default());
    state.block_settlement.store(true, Ordering::Release);
    let mut stream = operations
        .stream(
            Arc::new(TestProvider(state.clone())),
            request(),
            entry(&state, 0),
        )
        .unwrap();
    assert!(matches!(
        stream.next().await,
        Some(Ok(ProviderEvent::Usage { .. }))
    ));
    assert!(futures_util::poll!(stream.next()).is_pending());
    state.settlement_entered.notified().await;
    drop(stream);
    let mut settlement = Box::pin(operations.settle());
    assert!(futures_util::poll!(&mut settlement).is_pending());
    state.release.notify_one();
    settlement.await;
    let settled = state.settled.lock().unwrap();
    assert_eq!(settled.len(), 1);
    assert!(settled[0].1.terminal);
    assert_eq!(settled[0].1.usage.unwrap().output_tokens, 5);
}

#[tokio::test]
async fn failed_receipt_cannot_emit_successful_provider_terminal() {
    let operations = ProviderOperations::default();
    let state = Arc::new(State::default());
    state.fail_receipt.store(true, Ordering::Release);
    let mut stream = operations
        .stream(
            Arc::new(TestProvider(state.clone())),
            request(),
            entry(&state, 0),
        )
        .unwrap();
    assert!(matches!(
        stream.next().await,
        Some(Ok(ProviderEvent::Usage { .. }))
    ));
    assert!(matches!(stream.next().await, Some(Err(_))));
    assert!(stream.next().await.is_none());
    operations.settle().await;
}

#[tokio::test]
async fn failover_gets_a_new_attempt_and_rejection_prevents_provider_entry() {
    let state = Arc::new(State::default());
    state.reject_after_first.store(true, Ordering::Release);
    state.fail_provider.store(true, Ordering::Release);
    let provider: Arc<dyn Provider> = Arc::new(TestProvider(state.clone()));
    let router = crate::ProviderRouter::new(
        std::collections::BTreeMap::from([(
            "alias".into(),
            vec![
                "accounting-fixture/one".into(),
                "accounting-fixture/two".into(),
            ],
        )]),
        [provider],
        crate::RetryPolicy {
            max_attempts: 1,
            ..crate::RetryPolicy::default()
        },
    )
    .unwrap();
    let mut stream = router
        .stream_alias("alias", request(), Arc::new(Gate(state.clone())))
        .unwrap();
    assert!(matches!(
        stream.next().await,
        Some(Err(ProviderError {
            kind: ProviderErrorKind::InvalidRequest,
            ..
        }))
    ));
    assert_eq!(*state.entered.lock().unwrap(), vec![0, 1]);
    assert_eq!(state.invoked.load(Ordering::Acquire), 1);
    assert_eq!(state.settled.lock().unwrap().len(), 1);
    assert!(!state.settled.lock().unwrap()[0].1.terminal);
}
