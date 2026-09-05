//! Retains the provider actually invoked when a stream consumer disappears.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, Stream};
use tokio::sync::oneshot;

use crate::{
    BoxEventStream, ModelCandidate, Provider, ProviderAttempt, ProviderAttemptGate,
    ProviderAttemptOutcome, ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest,
};

const MAX_OPERATIONS: usize = 64;
type Completion = Shared<BoxFuture<'static, Result<(), ProviderError>>>;

#[derive(Clone, Default)]
pub(crate) struct ProviderOperations(Arc<Mutex<Vec<Arc<Operation>>>>);

#[derive(Default)]
struct Accounting {
    attempt: Option<Box<dyn ProviderAttempt>>,
    outcome: ProviderAttemptOutcome,
    invoked: bool,
}

pub(crate) struct AttemptEntry {
    pub(crate) candidate: ModelCandidate,
    pub(crate) gate: Arc<dyn ProviderAttemptGate>,
    pub(crate) number: u32,
}

struct Operation {
    provider: Arc<dyn Provider>,
    started: AtomicBool,
    completion: Completion,
}

impl ProviderOperations {
    pub(crate) fn stream(
        &self,
        provider: Arc<dyn Provider>,
        request: ProviderRequest,
        entry: AttemptEntry,
    ) -> Result<BoxEventStream, ProviderError> {
        let (finished, wait) = oneshot::channel();
        let completion = async move {
            if let Ok(result) = wait.await {
                result
            } else {
                Err(ProviderError::new(
                    ProviderErrorKind::EffectsUnsettled,
                    "provider cleanup owner exited without effect settlement proof",
                ))
            }
        }
        .boxed()
        .shared();
        let operation = Arc::new(Operation {
            provider: Arc::clone(&provider),
            started: AtomicBool::new(false),
            completion,
        });
        let mut operations = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for previous in operations
            .iter()
            .filter(|operation| operation.started.load(Ordering::Acquire))
        {
            if let Some(Err(error)) = previous.completion.clone().now_or_never() {
                return Err(error);
            }
        }
        if operations.len() >= MAX_OPERATIONS {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "provider operation admission is saturated by active or unsettled work",
            ));
        }
        operations.push(Arc::clone(&operation));
        drop(operations);
        let accounting = Arc::new(Mutex::new(Accounting::default()));
        let captured = accounting.clone();
        let invoked = provider;
        let inner = async_stream::try_stream! {
            let attempt = entry.gate.enter(&entry.candidate, &request, entry.number).await?;
            {
                let mut accounting = captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                accounting.attempt = Some(attempt);
                accounting.invoked = true;
            }
            let mut stream = invoked.stream(request).await?;
            while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
                {
                    let mut accounting = captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match &event {
                        Ok(ProviderEvent::Usage { usage }) => accounting.outcome.usage = Some(*usage),
                        Ok(ProviderEvent::Finished { .. }) => accounting.outcome.terminal = true,
                        Err(_) => accounting.outcome.terminal = false,
                        _ => {},
                    }
                }
                yield event?;
            }
        };
        Ok(Box::pin(OwnedProviderStream {
            inner: Some(Box::pin(inner)),
            completion: operation.completion.clone(),
            completion_reported: false,
            terminal: None,
            cleanup: Some(Cleanup {
                operation,
                operations: self.clone(),
                finished,
                accounting,
            }),
        }))
    }

    pub(crate) async fn settle(&self) -> Result<(), ProviderError> {
        loop {
            let pending = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|operation| operation.started.load(Ordering::Acquire))
                .map(|operation| operation.completion.clone())
                .collect::<Vec<_>>();
            if pending.is_empty() {
                return Ok(());
            }
            for result in futures_util::future::join_all(pending).await {
                result?;
            }
        }
    }
}

struct Cleanup {
    operation: Arc<Operation>,
    operations: ProviderOperations,
    finished: oneshot::Sender<Result<(), ProviderError>>,
    accounting: Arc<Mutex<Accounting>>,
}

impl Cleanup {
    fn begin(self) {
        self.operation.started.store(true, Ordering::Release);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!("provider cleanup has no runtime; effect settlement remains unproven");
            return;
        };
        runtime.spawn(async move {
            let (attempt, outcome, invoked) = {
                let mut accounting = self
                    .accounting
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    accounting.attempt.take(),
                    accounting.outcome,
                    accounting.invoked,
                )
            };
            let result = async {
                if invoked {
                    self.operation.provider.settle_effects().await?;
                }
                if let Some(attempt) = attempt {
                    attempt.settle(outcome).await?;
                }
                Ok(())
            }
            .await;
            // Completed entries retire themselves, even if no next request arrives.
            if result.is_ok() {
                self.operations
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|operation| !Arc::ptr_eq(operation, &self.operation));
            }
            let _ = self.finished.send(result);
        });
    }
}

struct OwnedProviderStream {
    inner: Option<BoxEventStream>,
    completion: Completion,
    completion_reported: bool,
    terminal: Option<ProviderEvent>,
    cleanup: Option<Cleanup>,
}

impl OwnedProviderStream {
    fn finish(&mut self) {
        let cleanup = self.cleanup.take();
        if let Some(cleanup) = &cleanup {
            cleanup.operation.started.store(true, Ordering::Release);
        }
        // A panicking destructor drops the local cleanup sender. Its registered
        // failed proof remains registered; it cannot look like an active invocation.
        self.inner.take();
        if let Some(cleanup) = cleanup {
            cleanup.begin();
        }
    }
}

impl Stream for OwnedProviderStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Some(inner) = &mut self.inner {
            match inner.as_mut().poll_next(context) {
                Poll::Ready(Some(Ok(event @ ProviderEvent::Finished { .. })))
                    if self.terminal.is_none() =>
                {
                    // A terminal is visible only after local effects and durable accounting settle.
                    self.terminal = Some(event);
                }
                Poll::Ready(Some(Ok(_))) if self.terminal.is_some() => {
                    if let Some(cleanup) = &self.cleanup {
                        cleanup
                            .accounting
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .outcome
                            .terminal = false;
                    }
                    self.terminal = None;
                    self.finish();
                    return Poll::Ready(Some(Err(ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "provider emitted data after its terminal",
                    ))));
                }
                Poll::Ready(Some(Err(error))) => {
                    self.terminal = None;
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => self.finish(),
                event => return event,
            }
        }
        if self.completion_reported {
            return Poll::Ready(None);
        }
        match self.completion.poll_unpin(context) {
            Poll::Ready(result) => {
                self.completion_reported = true;
                Poll::Ready(match result {
                    Ok(()) => self.terminal.take().map(Ok),
                    Err(error) => {
                        self.terminal = None;
                        Some(Err(error))
                    }
                })
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for OwnedProviderStream {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use async_trait::async_trait;
    use futures_util::StreamExt;
    use std::time::Duration;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct GatedProvider {
        panic_on_drop: bool,
        invoked: Notify,
        cleanup_started: Notify,
        release: Notify,
        settled: AtomicBool,
    }

    #[async_trait]
    impl Provider for GatedProvider {
        fn name(&self) -> &'static str {
            "gated"
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
        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            self.invoked.notify_one();
            if self.panic_on_drop {
                return Ok(Box::pin(PanicOnDrop));
            }
            std::future::pending().await
        }
        async fn settle_effects(&self) -> std::result::Result<(), crate::ProviderError> {
            self.cleanup_started.notify_one();
            self.release.notified().await;
            self.settled.store(true, Ordering::Release);
            Ok(())
        }
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: "fixture".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: crate::ToolChoice::None,
            max_output_tokens: 1,
            temperature: None,
            thinking: rw_types::config::ThinkingLevel::Off,
            cache_hint: None,
        }
    }

    struct PanicOnDrop;
    impl Stream for PanicOnDrop {
        type Item = Result<ProviderEvent, ProviderError>;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("provider destructor lost effect ownership");
        }
    }

    #[tokio::test]
    async fn panicking_stream_destructor_cannot_skip_the_registered_settlement_barrier() {
        let operations = ProviderOperations::default();
        let provider = Arc::new(GatedProvider {
            panic_on_drop: true,
            ..Default::default()
        });
        let mut stream = operations
            .stream(provider.clone(), request(), crate::attempt::fixture_entry())
            .expect("admitted provider");
        assert!(futures_util::poll!(stream.next()).is_pending());
        let task = tokio::spawn(async move {
            drop(stream);
        });
        assert!(task.await.unwrap_err().is_panic());
        let proof = tokio::time::timeout(Duration::from_secs(1), operations.settle())
            .await
            .expect("failed proof returns without hanging");
        assert_eq!(
            proof.expect_err("cleanup is unproven").kind,
            ProviderErrorKind::EffectsUnsettled
        );
        assert!(!provider.settled.load(Ordering::Acquire));
        assert_eq!(operations.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dropping_outer_future_retains_invoked_owner_until_cleanup_finishes() {
        let provider = Arc::new(GatedProvider::default());
        let operations = ProviderOperations::default();
        let mut stream = operations
            .stream(provider.clone(), request(), crate::attempt::fixture_entry())
            .expect("admit");
        let task = tokio::spawn(async move { stream.next().await });
        provider.invoked.notified().await;
        task.abort();
        let _ = task.await;
        provider.cleanup_started.notified().await;
        let mut settlement = tokio::spawn({
            let operations = operations.clone();
            async move { operations.settle().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut settlement)
                .await
                .is_err()
        );
        assert!(!provider.settled.load(Ordering::Acquire));
        provider.release.notify_one();
        settlement
            .await
            .expect("settlement task")
            .expect("effects settled");
        assert!(provider.settled.load(Ordering::Acquire));
        assert!(operations.0.lock().expect("registry").is_empty());
    }

    #[tokio::test]
    async fn admission_counts_abandoned_owners_and_completed_entries_self_retire() {
        let operations = ProviderOperations::default();
        let mut providers = Vec::new();
        for _ in 0..MAX_OPERATIONS {
            let provider = Arc::new(GatedProvider::default());
            let mut stream = operations
                .stream(provider.clone(), request(), crate::attempt::fixture_entry())
                .expect("admit");
            assert!(futures_util::poll!(stream.next()).is_pending());
            drop(stream);
            provider.cleanup_started.notified().await;
            providers.push(provider);
        }
        assert!(
            operations
                .stream(
                    Arc::new(GatedProvider::default()),
                    request(),
                    crate::attempt::fixture_entry()
                )
                .is_err()
        );
        for provider in providers {
            provider.release.notify_one();
        }
        operations.settle().await.expect("effects settled");
        assert!(operations.0.lock().expect("registry").is_empty());
    }
}

#[cfg(test)]
mod accounting_tests;
