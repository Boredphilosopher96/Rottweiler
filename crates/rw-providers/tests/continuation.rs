use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use futures_util::StreamExt;
use rw_providers::{
    BoxEventStream, CacheBreakpointSupport, Capabilities, ContinuationProvenance, FinishReason,
    Provider, ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest, ProviderRouter,
    Recorder, ReplayProvider, RetryPolicy, ToolChoice, WireMode,
};
use rw_types::{Block, Role, Turn, TurnMeta, config::ThinkingLevel};

#[path = "support/attempt_gate.rs"]
mod attempt_gate;

struct StatefulProvider {
    name: &'static str,
    fail: AtomicBool,
    calls: AtomicUsize,
    signatures: Mutex<Vec<String>>,
}

impl StatefulProvider {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            fail: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            signatures: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Provider for StatefulProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &str {
        self.name
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: false,
            vision: false,
            thinking: true,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        }
    }
    async fn continuation_provenance(
        &self,
    ) -> Result<Option<ContinuationProvenance>, ProviderError> {
        Ok(Some(ContinuationProvenance::bind(&[
            b"fixture-code-and-authority",
        ])))
    }
    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(ProviderError::new(
                ProviderErrorKind::Server,
                "fixture unavailable",
            ));
        }
        for block in request.turns.into_iter().flat_map(|turn| turn.blocks) {
            if let Block::Thinking {
                signature: Some(signature),
                ..
            } = block
            {
                self.signatures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(signature);
            }
        }
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::ThinkingDelta {
                content: "reason".to_owned(),
                signature: Some("adapter-payload".to_owned()),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

fn request(history: Option<&[ProviderEvent]>) -> ProviderRequest {
    let turns = history
        .into_iter()
        .flatten()
        .filter_map(|event| match event {
            ProviderEvent::ThinkingDelta { content, signature } => Some(Turn {
                role: Role::Assistant,
                blocks: vec![Block::Thinking {
                    content: content.clone(),
                    signature: signature.clone(),
                }],
                meta: TurnMeta::default(),
            }),
            _ => None,
        })
        .collect();
    ProviderRequest {
        model: "alias".to_owned(),
        turns,
        tools: vec![],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 64,
        temperature: None,
        thinking: ThinkingLevel::Low,
        cache_hint: None,
    }
}

fn router(
    providers: Vec<Arc<dyn Provider>>,
    routes: &[&str],
) -> Result<ProviderRouter, rw_providers::RouterError> {
    ProviderRouter::new(
        BTreeMap::from([(
            "alias".to_owned(),
            routes.iter().map(|value| (*value).to_owned()).collect(),
        )]),
        providers,
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        },
    )
}

async fn complete(
    router: &ProviderRouter,
    request: ProviderRequest,
) -> Result<Vec<ProviderEvent>, Box<dyn std::error::Error>> {
    Ok(router
        .stream_alias("alias", request, attempt_gate::gate())?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()?)
}

struct CountingGate(AtomicUsize);

#[async_trait]
impl rw_providers::ProviderAttemptGate for CountingGate {
    async fn enter(
        &self,
        candidate: &rw_providers::ModelCandidate,
        request: &ProviderRequest,
        attempt: u32,
    ) -> Result<Box<dyn rw_providers::ProviderAttempt>, ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        attempt_gate::gate()
            .enter(candidate, request, attempt)
            .await
    }
}

#[tokio::test]
async fn failover_never_sends_another_providers_continuation()
-> Result<(), Box<dyn std::error::Error>> {
    let first = StatefulProvider::new("first");
    let second = StatefulProvider::new("second");
    let router = router(
        vec![first.clone(), second.clone()],
        &["first/model", "second/model"],
    )?;
    let history = complete(&router, request(None)).await?;
    first.fail.store(true, Ordering::SeqCst);
    let gate = Arc::new(CountingGate(AtomicUsize::new(0)));
    let result = router
        .stream_alias("alias", request(Some(&history)), gate.clone())?
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(result.last(), Some(Err(error)) if error.kind == ProviderErrorKind::InvalidRequest)
    );
    assert_eq!(first.calls.load(Ordering::SeqCst), 2);
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
    assert_eq!(gate.0.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn routed_continuation_replays_with_its_recorded_provenance()
-> Result<(), Box<dyn std::error::Error>> {
    let directory =
        std::env::temp_dir().join(format!("rw-continuation-replay-{}", std::process::id()));
    if directory.exists() {
        std::fs::remove_dir_all(&directory)?;
    }
    let provider = StatefulProvider::new("recorded");
    let recorder = Arc::new(Recorder::new(
        provider.clone(),
        &directory,
        rw_providers::FixtureRedactor::default(),
    ));
    let live = router(vec![recorder.clone()], &["recorded/model"])?;
    let first = complete(&live, request(None)).await?;
    let second = complete(&live, request(Some(&first))).await?;
    assert_eq!(
        *provider
            .signatures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        ["adapter-payload"]
    );
    live.settle_effects().await?;
    recorder.flush().await?;
    let replay = Arc::new(ReplayProvider::load("recorded", &directory).await?);
    let replay = router(vec![replay], &["recorded/model"])?;
    let first_replayed = complete(&replay, request(None)).await?;
    assert_eq!(first, first_replayed);
    assert_eq!(
        second,
        complete(&replay, request(Some(&first_replayed))).await?
    );
    replay.settle_effects().await?;
    std::fs::remove_dir_all(directory)?;
    Ok(())
}

struct PreparingProvider {
    started: AtomicUsize,
    settled: AtomicUsize,
}

#[async_trait]
impl Provider for PreparingProvider {
    fn name(&self) -> &'static str {
        "preparing"
    }
    fn capabilities(&self) -> Capabilities {
        StatefulProvider::new("preparing").capabilities()
    }
    async fn continuation_provenance(
        &self,
    ) -> Result<Option<ContinuationProvenance>, ProviderError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
    async fn stream(&self, _: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "inference must not start before preparation finishes",
        ))
    }
    async fn settle_effects(&self) -> Result<(), ProviderError> {
        self.settled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn abandoned_provenance_preparation_keeps_the_provider_owned()
-> Result<(), Box<dyn std::error::Error>> {
    use futures_util::FutureExt;
    let provider = Arc::new(PreparingProvider {
        started: AtomicUsize::new(0),
        settled: AtomicUsize::new(0),
    });
    let router = router(vec![provider.clone()], &["preparing/model"])?;
    let mut stream = router.stream_alias("alias", request(None), attempt_gate::gate())?;
    assert!(stream.next().now_or_never().is_none());
    assert_eq!(provider.started.load(Ordering::SeqCst), 1);
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(1), router.settle_effects()).await??;
    assert_eq!(provider.settled.load(Ordering::SeqCst), 1);
    Ok(())
}
