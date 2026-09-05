mod redaction;
mod replay_reads;
mod writer;
pub use redaction::*;
use replay_reads::ReplayReads;
use writer::{RecordingWriter, WriterMessage};

use std::{
    collections::BTreeMap,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures_core::Stream;
use futures_util::FutureExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::types::RawSseFrame;
use crate::{
    BoxEventStream, CacheBreakpointSupport, Capabilities, Provider, ProviderError,
    ProviderErrorKind, ProviderEvent, ProviderModelMetadata, ProviderRequest, WireFrameSink,
    WireMode,
};

const FIXTURE_VERSION: u16 = 4;
const WRITER_QUEUE_CAPACITY: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordFixture {
    version: u16,
    provider: String,
    capabilities: RecordedCapabilities,
    #[serde(deserialize_with = "Option::deserialize")]
    model_metadata: Option<ProviderModelMetadata>,
    wire_mode: WireMode,
    request_hash: String,
    occurrence: u64,
    request: ProviderRequest,
    raw_sse: Vec<RawSseFrame>,
    #[serde(deserialize_with = "Option::deserialize")]
    start_error: Option<ProviderError>,
    items: Vec<RecordedItem>,
}

impl RecordFixture {
    fn validate(&self) -> Result<(), ProviderError> {
        if self.version != FIXTURE_VERSION
            || (self.start_error.is_some() && (!self.raw_sse.is_empty() || !self.items.is_empty()))
            || (self.wire_mode == WireMode::GitHubCopilot && self.start_error.is_none())
            || (self.wire_mode == WireMode::NormalizedReplay && !self.raw_sse.is_empty())
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "recording requires its schema, explicit stream dialect, and consistent outcome",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityManifest {
    version: u16,
    provider: String,
    capabilities: RecordedCapabilities,
    #[serde(deserialize_with = "Option::deserialize")]
    model_metadata: Option<ProviderModelMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedCapabilities {
    #[serde(deserialize_with = "Option::deserialize")]
    continuation_provenance: Option<crate::ContinuationProvenance>,
    tool_calling: bool,
    vision: bool,
    thinking: bool,
    cache_breakpoints: RecordedCacheBreakpointSupport,
    #[serde(deserialize_with = "Option::deserialize")]
    max_context_tokens: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    max_output_tokens: Option<u64>,
    wire_mode: WireMode,
    native_web_search: RecordedNativeWebSearchSupport,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct RecordedNativeWebSearchSupport(bool);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordedCacheBreakpointSupport {
    None,
    Explicit,
    Automatic,
}

impl From<&Capabilities> for RecordedCapabilities {
    fn from(value: &Capabilities) -> Self {
        Self {
            continuation_provenance: None,
            tool_calling: value.tool_calling,
            vision: value.vision,
            thinking: value.thinking,
            cache_breakpoints: match value.cache_breakpoints {
                CacheBreakpointSupport::None => RecordedCacheBreakpointSupport::None,
                CacheBreakpointSupport::Explicit => RecordedCacheBreakpointSupport::Explicit,
                CacheBreakpointSupport::Automatic => RecordedCacheBreakpointSupport::Automatic,
            },
            max_context_tokens: value.max_context_tokens,
            max_output_tokens: value.max_output_tokens,
            wire_mode: value.wire_mode,
            native_web_search: RecordedNativeWebSearchSupport::default(),
        }
    }
}

impl From<RecordedCapabilities> for Capabilities {
    fn from(value: RecordedCapabilities) -> Self {
        Self {
            tool_calling: value.tool_calling,
            vision: value.vision,
            thinking: value.thinking,
            cache_breakpoints: match value.cache_breakpoints {
                RecordedCacheBreakpointSupport::None => CacheBreakpointSupport::None,
                RecordedCacheBreakpointSupport::Explicit => CacheBreakpointSupport::Explicit,
                RecordedCacheBreakpointSupport::Automatic => CacheBreakpointSupport::Automatic,
            },
            max_context_tokens: value.max_context_tokens,
            max_output_tokens: value.max_output_tokens,
            wire_mode: value.wire_mode,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RecordedItem {
    Event { event: ProviderEvent },
    Error { error: ProviderError },
}

#[derive(Default)]
struct CapturedFrames(Mutex<Vec<RawSseFrame>>);

impl CapturedFrames {
    fn take(&self) -> Vec<RawSseFrame> {
        match self.0.lock() {
            Ok(mut frames) => std::mem::take(&mut *frames),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

impl WireFrameSink for CapturedFrames {
    fn capture(&self, event: Option<&str>, data: &str) {
        let frame = RawSseFrame {
            event: event.map(str::to_owned),
            data: data.to_owned(),
        };
        match self.0.lock() {
            Ok(mut frames) => frames.push(frame),
            Err(poisoned) => poisoned.into_inner().push(frame),
        }
    }
}

struct WriteJob {
    directory: PathBuf,
    provider: String,
    request_hash: String,
    occurrence: u64,
    fixture: RecordFixture,
    redactor: FixtureRedactor,
    completion: Option<oneshot::Sender<Result<(), ProviderError>>>,
}

impl WriteJob {
    fn write(&self) -> Result<(), ProviderError> {
        self.fixture.validate()?;
        ensure_capability_manifest(
            &self.directory,
            &self.provider,
            &self.request_hash,
            self.occurrence,
            &self.fixture.capabilities,
            self.fixture.model_metadata.as_ref(),
            self.fixture.start_error.is_some(),
        )?;
        write_fixture_sync(
            &self.directory,
            &self.provider,
            &self.request_hash,
            self.occurrence,
            &self.fixture,
            &self.redactor,
        )
    }
}

/// Middleware that records canonical requests and normalized stream output.
pub struct Recorder {
    inner: Arc<dyn Provider>,
    directory: PathBuf,
    redactor: FixtureRedactor,
    occurrences: Arc<Mutex<BTreeMap<String, u64>>>,
    activity: Arc<RecordingActivity>,
    writer: RecordingWriter,
    settlement_failed: AtomicBool,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Recorder")
            .field("provider", &self.inner.name())
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl Recorder {
    /// Wraps a live provider. Each request is written as one deterministic JSON fixture.
    #[must_use]
    pub fn new(
        inner: Arc<dyn Provider>,
        directory: impl Into<PathBuf>,
        redactor: FixtureRedactor,
    ) -> Self {
        Self::with_writer_capacity(inner, directory, redactor, WRITER_QUEUE_CAPACITY)
    }

    fn with_writer_capacity(
        inner: Arc<dyn Provider>,
        directory: impl Into<PathBuf>,
        redactor: FixtureRedactor,
        writer_capacity: usize,
    ) -> Self {
        Self {
            inner,
            directory: directory.into(),
            redactor,
            occurrences: Arc::new(Mutex::new(BTreeMap::new())),
            activity: Arc::new(RecordingActivity::default()),
            writer: RecordingWriter::new(writer_capacity),
            settlement_failed: AtomicBool::new(false),
        }
    }

    /// Waits until all streams returned by this recorder have written their fixtures.
    ///
    /// This is primarily useful after interrupting a stream. Dropping the stream
    /// performs an owned finalization with no detached task; this barrier also
    /// surfaces any filesystem error that could not be yielded to the dropped
    /// consumer.
    ///
    /// # Errors
    ///
    /// Returns the first fixture-write error observed since the previous flush.
    pub async fn flush(&self) -> Result<(), ProviderError> {
        self.activity.flush().await;
        self.writer.flush().await
    }

    fn recorded_capabilities(
        &self,
        capabilities: &Capabilities,
        provenance: Option<crate::ContinuationProvenance>,
    ) -> RecordedCapabilities {
        let mut recorded = RecordedCapabilities::from(capabilities);
        recorded.continuation_provenance = provenance;
        recorded.native_web_search = RecordedNativeWebSearchSupport(
            self.inner.native_web_search_capability()
                == crate::NativeWebSearchCapability::Supported,
        );
        recorded
    }
}

#[async_trait]
impl Provider for Recorder {
    async fn continuation_provenance(
        &self,
    ) -> Result<Option<crate::ContinuationProvenance>, ProviderError> {
        self.inner.continuation_provenance().await
    }

    async fn settle_effects(&self) -> std::result::Result<(), crate::ProviderError> {
        let inner = std::panic::AssertUnwindSafe(self.inner.settle_effects()).catch_unwind();
        let (inner, writer) = tokio::join!(inner, self.writer.settle());
        let result = inner
            .unwrap_or_else(|_| {
                Err(ProviderError::new(
                    ProviderErrorKind::EffectsUnsettled,
                    "recorded provider settlement panicked",
                ))
            })
            .and(writer);
        if result.is_err() {
            self.settlement_failed.store(true, Ordering::Release);
        }
        if self.settlement_failed.load(Ordering::Acquire) {
            return Err(ProviderError::new(
                ProviderErrorKind::EffectsUnsettled,
                "recorded provider did not prove local effect settlement",
            ));
        }
        result
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn native_web_search_capability(&self) -> crate::NativeWebSearchCapability {
        self.inner.native_web_search_capability()
    }

    async fn model_metadata(&self) -> Result<Option<crate::ProviderModelMetadata>, ProviderError> {
        self.inner.model_metadata().await
    }

    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        self.inner.cached_model_metadata()
    }

    fn cached_model_metadata_for(&self, model: &str) -> Option<ProviderModelMetadata> {
        self.inner.cached_model_metadata_for(model)
    }

    async fn discover_models(
        &self,
    ) -> Result<Option<crate::DiscoveredProviderCatalog>, ProviderError> {
        self.inner.discover_models().await
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        if self.settlement_failed.load(Ordering::Acquire) {
            return Err(ProviderError::new(
                ProviderErrorKind::EffectsUnsettled,
                "recorded provider admission is closed after failed settlement",
            ));
        }

        // Reserving bounded writer capacity happens before assigning an
        // occurrence or contacting the provider. Backpressure therefore
        // cannot create an unreplayable sequence hole, while every returned
        // stream owns a nonblocking enqueue permit for its Drop path.
        let permit = self.writer.reserve().await?;
        let request_hash = request_hash(&request)?;
        let provider = self.inner.name().to_owned();
        let occurrence_key = occurrence_key(&provider, &request_hash);
        // Reserve the occurrence at invocation time. In particular, a slow
        // provider handshake must not let a later call steal its sequence slot.
        let occurrence = next_occurrence(&self.occurrences, &occurrence_key);
        let metadata = async {
            let metadata = self.inner.model_metadata().await?;
            let provenance = self.inner.continuation_provenance().await?;
            Ok::<_, ProviderError>((metadata, provenance))
        }
        .await;
        let (model_metadata, provenance) = match metadata {
            Ok(metadata) => metadata,
            Err(error) => {
                let capabilities = self.inner.capabilities();
                let context = RecordingContext {
                    directory: self.directory.clone(),
                    provider,
                    capabilities: self.recorded_capabilities(&capabilities, None),
                    model_metadata: None,
                    wire_mode: capabilities.wire_mode,
                    request_hash,
                    occurrence,
                    request,
                    captured: Arc::new(CapturedFrames::default()),
                    redactor: self.redactor.clone(),
                    items: Vec::new(),
                    permit,
                };
                return Err(persist_start_error(context, error).await);
            }
        };
        let capabilities = model_metadata.as_ref().map_or_else(
            || self.inner.capabilities(),
            |value| value.capabilities.clone(),
        );
        let wire_mode = capabilities.wire_mode;
        let captured = Arc::new(CapturedFrames::default());
        let sink: Arc<dyn WireFrameSink> = captured.clone();
        self.activity.begin();
        let mut start = StartGuard {
            context: Some(RecordingContext {
                directory: self.directory.clone(),
                provider,
                capabilities: self.recorded_capabilities(&capabilities, provenance),
                model_metadata,
                wire_mode,
                request_hash,
                occurrence,
                request,
                captured,
                redactor: self.redactor.clone(),
                items: Vec::new(),
                permit,
            }),
            activity: Arc::clone(&self.activity),
            tracking: true,
        };
        if wire_mode == WireMode::GitHubCopilot {
            let context = start.context.take().ok_or_else(writer_state_error)?;
            start.finish_tracking();
            let error = ProviderError::new(
                ProviderErrorKind::Protocol,
                "provider stream requires an explicit wire dialect",
            );
            return Err(persist_start_error(context, error).await);
        }
        let inner_stream = match self
            .inner
            .stream_with_wire_sink(
                start
                    .context
                    .as_ref()
                    .map(|context| context.request.clone())
                    .ok_or_else(writer_state_error)?,
                sink,
            )
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                let context = start.context.take().ok_or_else(writer_state_error)?;
                start.finish_tracking();
                return Err(persist_start_error(context, error).await);
            }
        };
        let context = start.context.take().ok_or_else(writer_state_error)?;
        start.tracking = false;
        Ok(Box::pin(RecordingStream {
            inner: inner_stream,
            context: Some(context),
            activity: Arc::clone(&self.activity),
            completion: None,
            done: false,
        }))
    }
}

#[derive(Default)]
struct RecordingActivity {
    active: AtomicUsize,
    idle: Notify,
}

impl RecordingActivity {
    fn begin(&self) {
        self.active.fetch_add(1, Ordering::AcqRel);
    }

    fn finish(&self) {
        let previous = self.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "recording activity underflow");
        self.idle.notify_waiters();
    }

    async fn flush(&self) {
        loop {
            let idle = self.idle.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                break;
            }
            idle.await;
        }
    }
}

struct RecordingContext {
    directory: PathBuf,
    provider: String,
    capabilities: RecordedCapabilities,
    model_metadata: Option<ProviderModelMetadata>,
    wire_mode: WireMode,
    request_hash: String,
    occurrence: u64,
    request: ProviderRequest,
    captured: Arc<CapturedFrames>,
    redactor: FixtureRedactor,
    items: Vec<RecordedItem>,
    permit: mpsc::OwnedPermit<WriterMessage>,
}

impl RecordingContext {
    fn push(&mut self, item: &Result<ProviderEvent, ProviderError>) {
        match item {
            Ok(event) => self.items.push(RecordedItem::Event {
                event: event.clone(),
            }),
            Err(error) => self.items.push(RecordedItem::Error {
                error: error.clone(),
            }),
        }
    }

    fn enqueue_stream(
        mut self,
        interrupted: bool,
        with_completion: bool,
    ) -> Option<oneshot::Receiver<Result<(), ProviderError>>> {
        if interrupted {
            self.items.push(RecordedItem::Error {
                error: ProviderError::new(
                    ProviderErrorKind::Cancelled,
                    "provider stream was interrupted before completion",
                ),
            });
        }
        self.enqueue(None, with_completion)
    }

    fn enqueue_start_error(
        self,
        error: ProviderError,
    ) -> oneshot::Receiver<Result<(), ProviderError>> {
        self.enqueue(Some(error), true)
            .unwrap_or_else(|| unreachable!("completion was requested"))
    }

    fn enqueue(
        self,
        start_error: Option<ProviderError>,
        with_completion: bool,
    ) -> Option<oneshot::Receiver<Result<(), ProviderError>>> {
        let fixture = RecordFixture {
            version: FIXTURE_VERSION,
            provider: self.provider.clone(),
            capabilities: self.capabilities,
            model_metadata: self.model_metadata,
            wire_mode: self.wire_mode,
            request_hash: self.request_hash.clone(),
            occurrence: self.occurrence,
            request: self.request,
            raw_sse: self.captured.take(),
            start_error,
            items: self.items,
        };
        let (completion, receiver) = if with_completion {
            let (completion, receiver) = oneshot::channel();
            (Some(completion), Some(receiver))
        } else {
            (None, None)
        };
        self.permit.send(WriterMessage::Fixture(Box::new(WriteJob {
            directory: self.directory,
            provider: self.provider,
            request_hash: self.request_hash,
            occurrence: self.occurrence,
            fixture,
            redactor: self.redactor,
            completion,
        })));
        receiver
    }
}

struct StartGuard {
    context: Option<RecordingContext>,
    activity: Arc<RecordingActivity>,
    tracking: bool,
}

impl StartGuard {
    fn finish_tracking(&mut self) {
        if self.tracking {
            self.tracking = false;
            self.activity.finish();
        }
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            let error = ProviderError::new(
                ProviderErrorKind::Cancelled,
                "provider request was interrupted during stream startup",
            );
            let _ = context.enqueue(Some(error), false);
        }
        self.finish_tracking();
    }
}

async fn await_write(
    completion: oneshot::Receiver<Result<(), ProviderError>>,
) -> Result<(), ProviderError> {
    completion.await.map_err(|_| writer_state_error())?
}

async fn persist_start_error(context: RecordingContext, error: ProviderError) -> ProviderError {
    let completion = context.enqueue_start_error(error.clone());
    match await_write(completion).await {
        Ok(()) => error,
        Err(record_error) => ProviderError {
            // Retain the provider category so retry/failover behavior remains
            // identical when the diagnostic fixture cannot be written.
            kind: error.kind,
            message: format!(
                "{}; replay recording also failed: {}",
                error.message, record_error.message
            ),
            retry_after_ms: error.retry_after_ms,
        },
    }
}

fn writer_state_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        "replay fixture writer stopped before confirming the write",
    )
}

struct RecordingStream {
    inner: BoxEventStream,
    context: Option<RecordingContext>,
    activity: Arc<RecordingActivity>,
    completion: Option<oneshot::Receiver<Result<(), ProviderError>>>,
    done: bool,
}

impl RecordingStream {
    fn finalize(&mut self, interrupted: bool, with_completion: bool) {
        let Some(context) = self.context.take() else {
            return;
        };
        self.completion = context.enqueue_stream(interrupted, with_completion);
        self.activity.finish();
    }

    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<ProviderEvent, ProviderError>>> {
        let Some(completion) = &mut self.completion else {
            self.done = true;
            return Poll::Ready(None);
        };
        match Pin::new(completion).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(Ok(()))) => {
                self.completion = None;
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Ready(Ok(Err(error))) => {
                self.completion = None;
                self.done = true;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(Err(_)) => {
                self.completion = None;
                self.done = true;
                Poll::Ready(Some(Err(writer_state_error())))
            }
        }
    }
}

impl Stream for RecordingStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.context.is_none() {
            return self.poll_completion(cx);
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(item)) => {
                if let Some(context) = &mut self.context {
                    context.push(&item);
                }
                let terminal = item.is_err() || matches!(&item, Ok(ProviderEvent::Finished { .. }));
                if terminal {
                    self.finalize(false, true);
                }
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => {
                self.finalize(false, true);
                self.poll_completion(cx)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for RecordingStream {
    fn drop(&mut self) {
        if self.context.is_some() {
            self.finalize(true, false);
        }
    }
}

/// Provider that serves recorded streams without constructing an HTTP client.
#[derive(Clone, Debug)]
pub struct ReplayProvider {
    name: String,
    directory: PathBuf,
    capabilities: Capabilities,
    recorded_capabilities: RecordedCapabilities,
    model_metadata: Option<ProviderModelMetadata>,
    occurrences: Arc<Mutex<BTreeMap<String, u64>>>,
    reads: Arc<ReplayReads>,
}

impl ReplayProvider {
    /// Reads the persisted native-search capability without constructing a
    /// replay stream.
    ///
    /// # Errors
    ///
    /// Returns an error when the recording manifest or fixtures are missing,
    /// malformed, or inconsistent.
    pub async fn recorded_native_web_search_capability(
        name: &str,
        directory: &Path,
    ) -> Result<crate::NativeWebSearchCapability, ProviderError> {
        let (capabilities, _) = load_recorded_capabilities(directory, name).await?;
        Ok(if capabilities.native_web_search.0 {
            crate::NativeWebSearchCapability::Supported
        } else {
            crate::NativeWebSearchCapability::Unsupported
        })
    }

    /// Loads a network-free provider and validates the recorded capability
    /// manifest against every provider-scoped fixture before returning.
    ///
    /// # Errors
    ///
    /// Returns an error when no completed recording exists or any fixture has
    /// a different version, provider identity, wire mode, or capability set.
    pub async fn load(
        name: impl Into<String>,
        directory: impl Into<PathBuf>,
    ) -> Result<Self, ProviderError> {
        let name = name.into();
        let directory = directory.into();
        let (recorded_capabilities, model_metadata) =
            load_recorded_capabilities(&directory, &name).await?;
        let capabilities = recorded_capabilities.clone().into();
        Ok(Self {
            name,
            directory,
            capabilities,
            recorded_capabilities,
            model_metadata,
            occurrences: Arc::new(Mutex::new(BTreeMap::new())),
            reads: Arc::new(ReplayReads::default()),
        })
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    async fn settle_effects(&self) -> Result<(), ProviderError> {
        self.reads.settle().await
    }

    async fn continuation_provenance(
        &self,
    ) -> Result<Option<crate::ContinuationProvenance>, ProviderError> {
        Ok(self.recorded_capabilities.continuation_provenance.clone())
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn native_web_search_capability(&self) -> crate::NativeWebSearchCapability {
        if self.recorded_capabilities.native_web_search.0 {
            crate::NativeWebSearchCapability::Supported
        } else {
            crate::NativeWebSearchCapability::Unsupported
        }
    }

    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        Ok(self.model_metadata.clone())
    }

    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        self.model_metadata.clone()
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        let read = self.reads.begin()?;
        let hash = request_hash(&request)?;
        let occurrence_key = occurrence_key(&self.name, &hash);
        let occurrence = next_occurrence(&self.occurrences, &occurrence_key);
        let path = fixture_path(&self.directory, &self.name, &hash, occurrence);
        let bytes = read.read(path.clone()).await.map_err(|error| {
            if error.kind != ProviderErrorKind::ReplayMiss {
                return error;
            }
            ProviderError::new(
                ProviderErrorKind::ReplayMiss,
                format!(
                    "replay sequence for request {hash} is exhausted at occurrence {occurrence}; \
                     record another identical request or create a fresh replay provider to restart"
                ),
            )
        })?;
        let fixture: RecordFixture = serde_json::from_slice(&bytes).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                format!("invalid replay fixture {}: {error}", path.display()),
            )
        })?;
        fixture.validate()?;
        if fixture.provider != self.name
            || !fixture_matches_manifest(
                &fixture,
                &self.recorded_capabilities,
                self.model_metadata.as_ref(),
            )
            || fixture.request_hash != hash
            || fixture.occurrence != occurrence
        {
            return Err(ProviderError::new(
                ProviderErrorKind::ReplayMiss,
                "replay fixture version, provider, capabilities, canonical request, or occurrence does not match",
            ));
        }
        if let Some(error) = fixture.start_error {
            return Err(error);
        }
        let items = if fixture.raw_sse.is_empty() {
            recorded_to_results(fixture.items)
        } else {
            let mut parsed = match fixture.wire_mode {
                WireMode::AnthropicMessages => {
                    crate::anthropic::replay_sse_frames(&fixture.raw_sse)
                }
                WireMode::OpenAiChatCompletions => crate::openai::replay_sse_frames(
                    crate::OpenAiWireMode::ChatCompletions,
                    &fixture.raw_sse,
                ),
                WireMode::OpenAiResponses => crate::openai::replay_sse_frames(
                    crate::OpenAiWireMode::Responses,
                    &fixture.raw_sse,
                ),
                WireMode::GitHubCopilot | WireMode::NormalizedReplay => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "raw replay frames require an explicit provider wire dialect",
                    ));
                }
                WireMode::GitHubCopilotMessages => crate::github_copilot::replay_sse_frames(
                    crate::GitHubCopilotEndpoint::Messages,
                    &fixture.raw_sse,
                ),
                WireMode::GitHubCopilotResponses => crate::github_copilot::replay_sse_frames(
                    crate::GitHubCopilotEndpoint::Responses,
                    &fixture.raw_sse,
                ),
                WireMode::GitHubCopilotChatCompletions => crate::github_copilot::replay_sse_frames(
                    crate::GitHubCopilotEndpoint::ChatCompletions,
                    &fixture.raw_sse,
                ),
            };
            qualify_replayed_bound_identity(&mut parsed, &fixture.items, &self.name);
            reconcile_raw_replay(parsed, fixture.items)?
        };
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

fn qualify_replayed_bound_identity(
    parsed: &mut [Result<ProviderEvent, ProviderError>],
    recorded: &[RecordedItem],
    provider_name: &str,
) {
    for (parsed_item, recorded_item) in parsed.iter_mut().zip(recorded) {
        let (
            Ok(ProviderEvent::MessageStart {
                model: parsed_model,
            }),
            RecordedItem::Event {
                event:
                    ProviderEvent::MessageStart {
                        model: recorded_model,
                    },
            },
        ) = (parsed_item, recorded_item)
        else {
            continue;
        };
        if recorded_model == provider_name {
            recorded_model.clone_into(parsed_model);
        }
    }
}

fn recorded_to_results(items: Vec<RecordedItem>) -> Vec<Result<ProviderEvent, ProviderError>> {
    items
        .into_iter()
        .map(|item| match item {
            RecordedItem::Event { event } => Ok(event),
            RecordedItem::Error { error } => Err(error),
        })
        .collect()
}

fn results_to_recorded(items: &[Result<ProviderEvent, ProviderError>]) -> Vec<RecordedItem> {
    items
        .iter()
        .map(|item| match item {
            Ok(event) => RecordedItem::Event {
                event: event.clone(),
            },
            Err(error) => RecordedItem::Error {
                error: error.clone(),
            },
        })
        .collect()
}

fn reconcile_raw_replay(
    parsed: Vec<Result<ProviderEvent, ProviderError>>,
    recorded: Vec<RecordedItem>,
) -> Result<Vec<Result<ProviderEvent, ProviderError>>, ProviderError> {
    let reparsed = results_to_recorded(&parsed);
    if reparsed == recorded {
        return Ok(parsed);
    }

    // A transport can fail after complete SSE frames have already been
    // captured. Re-running only those frames quite correctly reports a
    // synthetic "stream ended early" protocol error; the live stream instead
    // observed the transport error. Validate the normalized prefix exactly,
    // then restore that recorded terminal error.
    let Some(RecordedItem::Error { error }) = recorded.last() else {
        return Err(raw_replay_mismatch());
    };
    if !error.is_retryable() && error.kind != ProviderErrorKind::Cancelled {
        return Err(raw_replay_mismatch());
    }
    let recorded_prefix = &recorded[..recorded.len() - 1];
    let parsed_prefix = match reparsed.last() {
        Some(RecordedItem::Error { error }) if is_incomplete_replay_error(error) => {
            &reparsed[..reparsed.len() - 1]
        }
        _ => reparsed.as_slice(),
    };
    if parsed_prefix != recorded_prefix {
        return Err(raw_replay_mismatch());
    }
    Ok(recorded_to_results(recorded))
}

fn is_incomplete_replay_error(error: &ProviderError) -> bool {
    error.kind == ProviderErrorKind::Protocol
        && matches!(
            error.message.as_str(),
            "Anthropic replay ended before message_stop"
                | "OpenAI replay ended before its terminal event"
        )
}

fn raw_replay_mismatch() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        "raw replay frames differ from the recorded event stream",
    )
}

async fn load_recorded_capabilities(
    directory: &Path,
    provider: &str,
) -> Result<(RecordedCapabilities, Option<ProviderModelMetadata>), ProviderError> {
    let manifest_path = capability_manifest_path(directory, provider);
    let manifest_bytes = tokio::fs::read(&manifest_path).await.map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::ReplayMiss,
            format!("no completed replay recording exists for provider {provider:?}"),
        )
    })?;
    let manifest: CapabilityManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                format!(
                    "invalid replay capability manifest {}: {error}",
                    manifest_path.display()
                ),
            )
        })?;
    validate_manifest(provider, &manifest)?;

    let prefix = format!("{}-", provider_hash(provider));
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .map_err(record_io_error)?;
    let mut fixture_paths = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(record_io_error)? {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with(&prefix)
            && file_name.ends_with(".json")
            && file_name != format!("{}-capabilities.json", provider_hash(provider))
        {
            fixture_paths.push(entry.path());
        }
    }
    fixture_paths.sort();
    if fixture_paths.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::ReplayMiss,
            format!("no completed replay fixtures exist for provider {provider:?}"),
        ));
    }
    for path in fixture_paths {
        let bytes = tokio::fs::read(&path).await.map_err(record_io_error)?;
        let fixture: RecordFixture = serde_json::from_slice(&bytes).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                format!("invalid replay fixture {}: {error}", path.display()),
            )
        })?;
        fixture.validate()?;
        if fixture.provider != provider
            || !fixture_matches_manifest(
                &fixture,
                &manifest.capabilities,
                manifest.model_metadata.as_ref(),
            )
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                format!(
                    "replay fixture {} is inconsistent with its provider capability manifest",
                    path.display()
                ),
            ));
        }
    }
    Ok((manifest.capabilities, manifest.model_metadata))
}

fn ensure_capability_manifest(
    directory: &Path,
    provider: &str,
    hash: &str,
    occurrence: u64,
    capabilities: &RecordedCapabilities,
    model_metadata: Option<&ProviderModelMetadata>,
    is_start_error: bool,
) -> Result<(), ProviderError> {
    std::fs::create_dir_all(directory).map_err(record_io_error)?;
    if let Some(metadata) = model_metadata {
        validate_metadata_capabilities(capabilities, metadata)?;
    }
    let manifest = CapabilityManifest {
        version: FIXTURE_VERSION,
        provider: provider.to_owned(),
        capabilities: capabilities.clone(),
        model_metadata: model_metadata.cloned(),
    };
    let target = capability_manifest_path(directory, provider);
    if target.exists() {
        return reconcile_capability_manifest(
            &target,
            provider,
            capabilities,
            model_metadata,
            is_start_error,
            hash,
            occurrence,
        );
    }
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("could not serialize replay capability manifest: {error}"),
        )
    })?;
    let provider_hash = provider_hash(provider);
    let temporary = directory.join(format!(
        ".{provider_hash}-capabilities-{hash}-{occurrence:08}.tmp"
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut output = options.open(&temporary).map_err(record_io_error)?;
    output.write_all(&bytes).map_err(record_io_error)?;
    output.sync_all().map_err(record_io_error)?;
    drop(output);
    match std::fs::hard_link(&temporary, &target) {
        Ok(()) => {
            std::fs::remove_file(&temporary).map_err(record_io_error)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&temporary).map_err(record_io_error)?;
            reconcile_capability_manifest(
                &target,
                provider,
                capabilities,
                model_metadata,
                is_start_error,
                hash,
                occurrence,
            )
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(record_io_error(error))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_capability_manifest(
    path: &Path,
    provider: &str,
    capabilities: &RecordedCapabilities,
    model_metadata: Option<&ProviderModelMetadata>,
    is_start_error: bool,
    hash: &str,
    occurrence: u64,
) -> Result<(), ProviderError> {
    let bytes = std::fs::read(path).map_err(record_io_error)?;
    let manifest: CapabilityManifest = serde_json::from_slice(&bytes).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("invalid replay capability manifest: {error}"),
        )
    })?;
    validate_manifest(provider, &manifest)?;
    match (manifest.model_metadata.as_ref(), model_metadata) {
        (None, Some(metadata)) => {
            validate_metadata_capabilities(capabilities, metadata)?;
            if !wire_mode_evolves_from_unresolved(
                manifest.capabilities.wire_mode,
                capabilities.wire_mode,
            ) {
                return Err(changed_manifest_error(provider));
            }
            let resolved = CapabilityManifest {
                version: FIXTURE_VERSION,
                provider: provider.to_owned(),
                capabilities: capabilities.clone(),
                model_metadata: Some(metadata.clone()),
            };
            replace_manifest_atomically(path, &resolved, hash, occurrence)
        }
        (Some(existing), Some(incoming))
            if existing == incoming && &manifest.capabilities == capabilities =>
        {
            Ok(())
        }
        (Some(_), None)
            if is_start_error
                && wire_mode_evolves_from_unresolved(
                    capabilities.wire_mode,
                    manifest.capabilities.wire_mode,
                ) =>
        {
            // A transient discovery failure may be recorded after a resolved
            // manifest. It cannot downgrade the final catalog snapshot.
            Ok(())
        }
        (None, None) if &manifest.capabilities == capabilities => Ok(()),
        _ => Err(changed_manifest_error(provider)),
    }
}

fn fixture_matches_manifest(
    fixture: &RecordFixture,
    manifest_capabilities: &RecordedCapabilities,
    manifest_metadata: Option<&ProviderModelMetadata>,
) -> bool {
    let exact = &fixture.capabilities == manifest_capabilities
        && fixture.model_metadata.as_ref() == manifest_metadata
        && fixture.wire_mode == manifest_capabilities.wire_mode;
    if exact {
        return true;
    }
    manifest_metadata.is_some()
        && fixture.model_metadata.is_none()
        && fixture.start_error.is_some()
        && fixture.raw_sse.is_empty()
        && fixture.items.is_empty()
        && fixture.wire_mode == fixture.capabilities.wire_mode
        && wire_mode_evolves_from_unresolved(
            fixture.capabilities.wire_mode,
            manifest_capabilities.wire_mode,
        )
}

fn wire_mode_evolves_from_unresolved(unresolved: WireMode, resolved: WireMode) -> bool {
    unresolved == resolved
        || matches!(
            (unresolved, resolved),
            (
                WireMode::GitHubCopilot,
                WireMode::GitHubCopilotMessages
                    | WireMode::GitHubCopilotResponses
                    | WireMode::GitHubCopilotChatCompletions
            )
        )
}

fn validate_metadata_capabilities(
    capabilities: &RecordedCapabilities,
    metadata: &ProviderModelMetadata,
) -> Result<(), ProviderError> {
    let mut metadata_capabilities = RecordedCapabilities::from(&metadata.capabilities);
    metadata_capabilities
        .continuation_provenance
        .clone_from(&capabilities.continuation_provenance);
    metadata_capabilities.native_web_search = capabilities.native_web_search;
    if metadata_capabilities != *capabilities {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "provider model metadata capabilities do not match the recorded capabilities",
        ));
    }
    Ok(())
}

fn replace_manifest_atomically(
    path: &Path,
    manifest: &CapabilityManifest,
    hash: &str,
    occurrence: u64,
) -> Result<(), ProviderError> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("could not serialize resolved replay capability manifest: {error}"),
        )
    })?;
    let temporary = path.with_file_name(format!(
        ".{}-resolved-{hash}-{occurrence:08}.tmp",
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("capabilities")
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut output = options.open(&temporary).map_err(record_io_error)?;
    output.write_all(&bytes).map_err(record_io_error)?;
    output.sync_all().map_err(record_io_error)?;
    drop(output);
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        record_io_error(error)
    })
}

fn changed_manifest_error(provider: &str) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        format!("provider {provider:?} changed model metadata within one recording directory"),
    )
}

fn validate_manifest(provider: &str, manifest: &CapabilityManifest) -> Result<(), ProviderError> {
    if manifest.version != FIXTURE_VERSION || manifest.provider != provider {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "replay capability manifest version or provider does not match",
        ));
    }
    if manifest.model_metadata.as_ref().is_some_and(|metadata| {
        let mut metadata_capabilities = RecordedCapabilities::from(&metadata.capabilities);
        metadata_capabilities
            .continuation_provenance
            .clone_from(&manifest.capabilities.continuation_provenance);
        metadata_capabilities.native_web_search = manifest.capabilities.native_web_search;
        metadata_capabilities != manifest.capabilities
    }) {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "replay model metadata capabilities do not match the capability manifest",
        ));
    }
    Ok(())
}

fn write_fixture_sync(
    directory: &Path,
    provider: &str,
    hash: &str,
    occurrence: u64,
    fixture: &RecordFixture,
    redactor: &FixtureRedactor,
) -> Result<(), ProviderError> {
    std::fs::create_dir_all(directory).map_err(record_io_error)?;
    let bytes = serde_json::to_string_pretty(fixture).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("could not serialize replay fixture: {error}"),
        )
    })?;
    let redacted = redactor.redact(&bytes);
    if redacted.len() > replay_reads::MAX_FIXTURE_BYTES {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "recording fixture exceeds encoded byte admission",
        ));
    }
    let target = fixture_path(directory, provider, hash, occurrence);
    let provider_hash = provider_hash(provider);
    let temporary = directory.join(format!(".{provider_hash}-{hash}-{occurrence:08}.tmp"));
    std::fs::write(&temporary, redacted).map_err(record_io_error)?;
    std::fs::rename(&temporary, &target).map_err(record_io_error)
}

#[allow(clippy::needless_pass_by_value)]
fn record_io_error(error: std::io::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        format!("could not write replay fixture: {error}"),
    )
}

fn fixture_path(directory: &Path, provider: &str, hash: &str, occurrence: u64) -> PathBuf {
    let provider_hash = provider_hash(provider);
    directory.join(format!("{provider_hash}-{hash}-{occurrence:08}.json"))
}

fn capability_manifest_path(directory: &Path, provider: &str) -> PathBuf {
    directory.join(format!("{}-capabilities.json", provider_hash(provider)))
}

fn occurrence_key(provider: &str, hash: &str) -> String {
    format!("{}:{hash}", provider_hash(provider))
}

fn provider_hash(provider: &str) -> String {
    blake3::hash(provider.as_bytes()).to_hex().to_string()
}

fn next_occurrence(occurrences: &Mutex<BTreeMap<String, u64>>, hash: &str) -> u64 {
    let mut occurrences = occurrences
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let next = occurrences.entry(hash.to_owned()).or_default();
    let occurrence = *next;
    *next = next.saturating_add(1);
    occurrence
}

fn request_hash(request: &ProviderRequest) -> Result<String, ProviderError> {
    let value = serde_json::to_value(request).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("could not canonicalize provider request: {error}"),
        )
    })?;
    let canonical = canonicalize(value);
    let bytes = serde_json::to_vec(&canonical).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("could not encode canonical provider request: {error}"),
        )
    })?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize).collect())
        }
        scalar => scalar,
    }
}

#[cfg(test)]
mod tests;
