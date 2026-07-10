use std::{
    collections::BTreeMap,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};

use crate::types::RawSseFrame;
use crate::{
    BoxEventStream, CacheBreakpointSupport, Capabilities, Provider, ProviderError,
    ProviderErrorKind, ProviderEvent, ProviderModelMetadata, ProviderRequest, WireFrameSink,
    WireMode,
};

const FIXTURE_VERSION: u16 = 4;
const WRITER_QUEUE_CAPACITY: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RecordFixture {
    version: u16,
    provider: String,
    capabilities: RecordedCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_metadata: Option<ProviderModelMetadata>,
    wire_mode: WireMode,
    request_hash: String,
    occurrence: u64,
    request: ProviderRequest,
    #[serde(default)]
    raw_sse: Vec<RawSseFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_error: Option<ProviderError>,
    #[serde(default)]
    items: Vec<RecordedItem>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CapabilityManifest {
    version: u16,
    provider: String,
    capabilities: RecordedCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_metadata: Option<ProviderModelMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct RecordedCapabilities {
    tool_calling: bool,
    vision: bool,
    thinking: bool,
    cache_breakpoints: RecordedCacheBreakpointSupport,
    max_context_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
    wire_mode: WireMode,
}

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

/// Shared known-secret redactor applied before fixture bytes reach disk.
///
/// Clones share one registry so credentials learned after provider composition
/// (for example, refreshed OAuth tokens) are visible to an already-created
/// recorder before it serializes a response.
#[derive(Clone, Default)]
pub struct FixtureRedactor {
    secrets: Arc<std::sync::RwLock<Vec<String>>>,
}

impl std::fmt::Debug for FixtureRedactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FixtureRedactor")
            .field("registered_secret_count", &self.registered_secret_count())
            .finish_non_exhaustive()
    }
}

impl FixtureRedactor {
    /// Creates a redactor from registered secrets. Empty values are ignored.
    #[must_use]
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let redactor = Self::default();
        for secret in secrets {
            redactor.register_value(secret);
        }
        redactor
    }

    /// Registers a credential without exposing it through the type system.
    /// Empty values are ignored and duplicate registrations are deduplicated.
    pub fn register_secret(&self, secret: &crate::Secret) {
        self.register_value(secret.expose_secret().to_owned());
    }

    /// Number of non-empty known secrets registered for fixture sanitization.
    /// This exposes no credential material and supports acceptance assertions
    /// that every preflighted credential reached the recording boundary.
    #[must_use]
    pub fn registered_secret_count(&self) -> usize {
        match self.secrets.read() {
            Ok(secrets) => secrets.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Whether already-rendered content still contains a registered secret.
    /// The result exposes no secret value.
    #[must_use]
    pub fn contains_registered_secret(&self, value: &str) -> bool {
        match self.secrets.read() {
            Ok(secrets) => secrets.iter().any(|secret| value.contains(secret)),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .any(|secret| value.contains(secret)),
        }
    }

    fn redact(&self, value: &str) -> String {
        let redact_with = |secrets: &[String]| {
            secrets.iter().fold(value.to_owned(), |rendered, secret| {
                rendered.replace(secret, "[REDACTED]")
            })
        };
        match self.secrets.read() {
            Ok(secrets) => redact_with(&secrets),
            Err(poisoned) => redact_with(&poisoned.into_inner()),
        }
    }

    fn register_value(&self, secret: String) {
        if secret.is_empty() {
            return;
        }
        let mut secrets = match self.secrets.write() {
            Ok(secrets) => secrets,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !secrets.contains(&secret) {
            secrets.push(secret);
        }
    }
}

impl crate::KnownSecretRegistrar for FixtureRedactor {
    fn register(&self, secret: &crate::Secret) {
        self.register_secret(secret);
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

enum WriterMessage {
    Fixture(Box<WriteJob>),
    Barrier(oneshot::Sender<Result<(), ProviderError>>),
}

struct RecordingWriter {
    sender: mpsc::Sender<WriterMessage>,
    receiver: Mutex<Option<mpsc::Receiver<WriterMessage>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl RecordingWriter {
    fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
            worker: Mutex::new(None),
        }
    }

    fn start(&self) {
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(mut receiver) = receiver else {
            return;
        };
        let worker = tokio::task::spawn_blocking(move || {
            let mut first_error = None;
            while let Some(message) = receiver.blocking_recv() {
                match message {
                    WriterMessage::Fixture(mut job) => {
                        let result = job.write();
                        if first_error.is_none() {
                            first_error = result.as_ref().err().cloned();
                        }
                        if let Some(completion) = job.completion.take() {
                            let _ = completion.send(result);
                        }
                    }
                    WriterMessage::Barrier(completion) => {
                        let result = first_error.take().map_or(Ok(()), Err);
                        let _ = completion.send(result);
                    }
                }
            }
        });
        *self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(worker);
    }

    async fn reserve(&self) -> Result<mpsc::OwnedPermit<WriterMessage>, ProviderError> {
        self.start();
        self.sender
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| self.unavailable_error())
    }

    async fn flush(&self) -> Result<(), ProviderError> {
        self.start();
        let (completion, result) = oneshot::channel();
        self.sender
            .send(WriterMessage::Barrier(completion))
            .await
            .map_err(|_| self.unavailable_error())?;
        result.await.map_err(|_| self.unavailable_error())?
    }

    fn unavailable_error(&self) -> ProviderError {
        let worker_finished = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(JoinHandle::is_finished);
        ProviderError::new(
            ProviderErrorKind::Protocol,
            if worker_finished {
                "replay fixture writer stopped unexpectedly"
            } else {
                "replay fixture writer is unavailable"
            },
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
}

#[async_trait]
impl Provider for Recorder {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn model_metadata(&self) -> Result<Option<crate::ProviderModelMetadata>, ProviderError> {
        self.inner.model_metadata().await
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
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
        let model_metadata = match self.inner.model_metadata().await {
            Ok(metadata) => metadata,
            Err(error) => {
                let capabilities = self.inner.capabilities();
                let context = RecordingContext {
                    directory: self.directory.clone(),
                    provider,
                    capabilities: RecordedCapabilities::from(&capabilities),
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
                capabilities: RecordedCapabilities::from(&capabilities),
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
    model_metadata: Option<ProviderModelMetadata>,
    occurrences: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl ReplayProvider {
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
        let (capabilities, model_metadata) = load_recorded_capabilities(&directory, &name).await?;
        Ok(Self {
            name,
            directory,
            capabilities,
            model_metadata,
            occurrences: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        Ok(self.model_metadata.clone())
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        let hash = request_hash(&request)?;
        let occurrence_key = occurrence_key(&self.name, &hash);
        let occurrence = next_occurrence(&self.occurrences, &occurrence_key);
        let path = fixture_path(&self.directory, &self.name, &hash, occurrence);
        let bytes = tokio::fs::read(&path).await.map_err(|_| {
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
        if fixture.version != FIXTURE_VERSION
            || fixture.provider != self.name
            || !fixture_matches_manifest(
                &fixture,
                &RecordedCapabilities::from(&self.capabilities),
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
            if !fixture.raw_sse.is_empty() || !fixture.items.is_empty() {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "provider-start replay fixture also contained stream output",
                ));
            }
            return Err(error);
        }
        let items = if fixture.raw_sse.is_empty() {
            recorded_to_results(fixture.items)
        } else {
            let parsed = match fixture.wire_mode {
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
                // Version-4 fixtures written before exact Copilot dialects were
                // persisted keep their already-normalized items. This is an
                // explicit compatibility path and never guesses from frames.
                WireMode::GitHubCopilot | WireMode::NormalizedReplay => {
                    recorded_to_results(fixture.items.clone())
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
            reconcile_raw_replay(parsed, fixture.items)?
        };
        Ok(Box::pin(futures_util::stream::iter(items)))
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
        "raw replay frames no longer normalize to the recorded event stream",
    )
}

async fn load_recorded_capabilities(
    directory: &Path,
    provider: &str,
) -> Result<(Capabilities, Option<ProviderModelMetadata>), ProviderError> {
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
        if fixture.version != FIXTURE_VERSION
            || fixture.provider != provider
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
    Ok((manifest.capabilities.into(), manifest.model_metadata))
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
            let upgraded = CapabilityManifest {
                version: FIXTURE_VERSION,
                provider: provider.to_owned(),
                capabilities: capabilities.clone(),
                model_metadata: Some(metadata.clone()),
            };
            replace_manifest_atomically(path, &upgraded, hash, occurrence)
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
    if RecordedCapabilities::from(&metadata.capabilities) != *capabilities {
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
            format!("could not serialize upgraded replay capability manifest: {error}"),
        )
    })?;
    let temporary = path.with_file_name(format!(
        ".{}-upgrade-{hash}-{occurrence:08}.tmp",
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
        RecordedCapabilities::from(&metadata.capabilities) != manifest.capabilities
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
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use async_trait::async_trait;
    use futures_util::StreamExt;
    use tokio::sync::Notify;

    use crate::types::RawSseFrame;
    use crate::{
        BoxEventStream, CacheBreakpointSupport, Capabilities, FinishReason, Provider,
        ProviderError, ProviderErrorKind, ProviderEvent, ProviderModelMetadata, ProviderRequest,
        ThinkingLevel, TokenUsage, ToolChoice, UsageAccounting, WireFrameSink, WireMode,
    };

    use super::{
        FixtureRedactor, RecordFixture, Recorder, ReplayProvider, fixture_path, request_hash,
    };

    struct FixtureProvider {
        name: String,
    }

    struct SequenceProvider {
        calls: AtomicUsize,
    }

    struct StartErrorProvider;

    struct MetadataErrorProvider;

    struct FlakyMetadataProvider {
        metadata_calls: AtomicUsize,
    }

    struct LegacyCopilotRawProvider;

    struct ResolvedMetadataStartErrorProvider;

    struct RawPrefixProvider;

    struct InterruptibleRawProvider;

    struct RestrictedProvider;

    struct DelayedStartProvider {
        calls: AtomicUsize,
        first_entered: Arc<Notify>,
        release_first: Arc<Notify>,
    }

    #[test]
    fn shared_redactor_debug_is_safe_and_poisoning_retains_registered_secrets() {
        const FIRST: &str = "redactor-debug-canary-one";
        const SECOND: &str = "redactor-debug-canary-two";
        let redactor = FixtureRedactor::default();
        redactor.register_secret(&crate::Secret::new(FIRST));
        let lock = Arc::clone(&redactor.secrets);
        let _ = std::panic::catch_unwind(move || {
            let _guard = lock
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            panic!("deliberately poison redactor registry");
        });

        redactor.register_secret(&crate::Secret::new(SECOND));
        let rendered = redactor.redact(&format!("{FIRST} {SECOND}"));
        assert_eq!(rendered, "[REDACTED] [REDACTED]");
        let debug = format!("{redactor:?}");
        assert!(!debug.contains(FIRST));
        assert!(!debug.contains(SECOND));
        assert_eq!(redactor.registered_secret_count(), 2);
    }

    #[async_trait]
    impl Provider for SequenceProvider {
        fn name(&self) -> &'static str {
            "sequence"
        }

        fn capabilities(&self) -> Capabilities {
            test_capabilities()
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(Box::pin(futures_util::stream::iter([
                Ok(ProviderEvent::TextDelta {
                    text: format!("response-{call}"),
                }),
                Ok(ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    #[async_trait]
    impl Provider for StartErrorProvider {
        fn name(&self) -> &'static str {
            "start-error"
        }

        fn capabilities(&self) -> Capabilities {
            test_capabilities()
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            Err(
                ProviderError::new(ProviderErrorKind::RateLimited, "fixture rate limit")
                    .with_retry_after(1_250),
            )
        }
    }

    #[async_trait]
    impl Provider for MetadataErrorProvider {
        fn name(&self) -> &'static str {
            "metadata-error"
        }

        fn capabilities(&self) -> Capabilities {
            test_capabilities()
        }

        async fn model_metadata(
            &self,
        ) -> Result<Option<crate::ProviderModelMetadata>, ProviderError> {
            Err(ProviderError::new(
                ProviderErrorKind::Server,
                "fixture metadata discovery failed",
            ))
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            panic!("stream must not run after metadata discovery failure")
        }
    }

    #[async_trait]
    impl Provider for FlakyMetadataProvider {
        fn name(&self) -> &'static str {
            "flaky-metadata"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                wire_mode: WireMode::GitHubCopilot,
                ..test_capabilities()
            }
        }

        async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
            if self.metadata_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err(ProviderError::new(
                    ProviderErrorKind::Server,
                    "fixture transient metadata failure",
                ));
            }
            Ok(Some(flaky_metadata()))
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            Ok(flaky_metadata_items(None))
        }

        async fn stream_with_wire_sink(
            &self,
            _request: ProviderRequest,
            sink: Arc<dyn WireFrameSink>,
        ) -> Result<BoxEventStream, ProviderError> {
            Ok(flaky_metadata_items(Some(&sink)))
        }
    }

    #[async_trait]
    impl Provider for LegacyCopilotRawProvider {
        fn name(&self) -> &'static str {
            "legacy-copilot"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                wire_mode: WireMode::GitHubCopilot,
                ..test_capabilities()
            }
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            Ok(legacy_copilot_items())
        }

        async fn stream_with_wire_sink(
            &self,
            _request: ProviderRequest,
            sink: Arc<dyn WireFrameSink>,
        ) -> Result<BoxEventStream, ProviderError> {
            // Deliberately ambiguous/error-shaped raw data proves the legacy
            // compatibility path replays recorded normalized items, not a
            // guessed Copilot dialect.
            sink.capture(None, r#"{"error":{"type":"future_error"}}"#);
            Ok(legacy_copilot_items())
        }
    }

    #[async_trait]
    impl Provider for ResolvedMetadataStartErrorProvider {
        fn name(&self) -> &'static str {
            "resolved-metadata-start-error"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                wire_mode: WireMode::GitHubCopilot,
                ..test_capabilities()
            }
        }

        async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
            Ok(Some(flaky_metadata()))
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            Err(ProviderError::new(
                ProviderErrorKind::Timeout,
                "fixture resolved provider start timeout",
            ))
        }
    }

    fn flaky_metadata() -> ProviderModelMetadata {
        ProviderModelMetadata {
            capabilities: Capabilities {
                tool_calling: true,
                vision: true,
                thinking: true,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: Some(200_000),
                max_output_tokens: Some(32_000),
                wire_mode: WireMode::GitHubCopilotResponses,
            },
            pricing: None,
            accounting: UsageAccounting::AiCredits {
                micros_usd_per_credit: 10_000,
            },
        }
    }

    fn flaky_metadata_items(sink: Option<&Arc<dyn WireFrameSink>>) -> BoxEventStream {
        let frames = [
            (
                "response.created",
                r#"{"response":{"model":"gpt-fixture"}}"#,
            ),
            ("response.output_text.delta", r#"{"delta":"resolved"}"#),
            ("response.completed", r#"{"response":{"usage":{}}}"#),
        ];
        if let Some(sink) = sink {
            for (event, data) in frames {
                sink.capture(Some(event), data);
            }
        }
        let raw = frames
            .into_iter()
            .map(|(event, data)| RawSseFrame {
                event: Some(event.to_owned()),
                data: data.to_owned(),
            })
            .collect::<Vec<_>>();
        Box::pin(futures_util::stream::iter(
            crate::github_copilot::replay_sse_frames(crate::GitHubCopilotEndpoint::Responses, &raw),
        ))
    }

    fn legacy_copilot_items() -> BoxEventStream {
        Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::TextDelta {
                text: "legacy-normalized".to_owned(),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ]))
    }

    #[async_trait]
    impl Provider for RawPrefixProvider {
        fn name(&self) -> &'static str {
            "raw-prefix"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                wire_mode: WireMode::AnthropicMessages,
                ..test_capabilities()
            }
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            Ok(raw_prefix_items())
        }

        async fn stream_with_wire_sink(
            &self,
            _request: ProviderRequest,
            sink: Arc<dyn WireFrameSink>,
        ) -> Result<BoxEventStream, ProviderError> {
            sink.capture(
                Some("message_start"),
                r#"{"type":"message_start","message":{"model":"fixture-model","usage":{"input_tokens":3}}}"#,
            );
            Ok(raw_prefix_items())
        }
    }

    fn raw_prefix_items() -> BoxEventStream {
        Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::MessageStart {
                model: "fixture-model".to_owned(),
            }),
            Ok(ProviderEvent::Usage {
                usage: TokenUsage {
                    input_tokens: 3,
                    ..TokenUsage::default()
                },
            }),
            Err(ProviderError::new(
                ProviderErrorKind::Network,
                "fixture connection reset",
            )),
        ]))
    }

    #[async_trait]
    impl Provider for InterruptibleRawProvider {
        fn name(&self) -> &'static str {
            "interruptible-raw"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                wire_mode: WireMode::AnthropicMessages,
                ..test_capabilities()
            }
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            Ok(interruptible_raw_items(None))
        }

        async fn stream_with_wire_sink(
            &self,
            _request: ProviderRequest,
            sink: Arc<dyn WireFrameSink>,
        ) -> Result<BoxEventStream, ProviderError> {
            Ok(interruptible_raw_items(Some(sink)))
        }
    }

    #[async_trait]
    impl Provider for RestrictedProvider {
        fn name(&self) -> &'static str {
            "restricted"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tool_calling: false,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::Automatic,
                max_context_tokens: Some(2_048),
                max_output_tokens: Some(64),
                wire_mode: WireMode::NormalizedReplay,
            }
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            Ok(Box::pin(futures_util::stream::iter([
                Ok(ProviderEvent::TextDelta {
                    text: "restricted".to_owned(),
                }),
                Ok(ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    fn interruptible_raw_items(sink: Option<Arc<dyn WireFrameSink>>) -> BoxEventStream {
        Box::pin(async_stream::stream! {
            if let Some(sink) = &sink {
                sink.capture(
                    Some("content_block_delta"),
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"prefix"}}"#,
                );
            }
            yield Ok(ProviderEvent::TextDelta {
                text: "prefix".to_owned(),
            });
            if let Some(sink) = &sink {
                sink.capture(Some("message_stop"), r#"{"type":"message_stop"}"#);
            }
            yield Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            });
        })
    }

    #[async_trait]
    impl Provider for DelayedStartProvider {
        fn name(&self) -> &'static str {
            "delayed"
        }

        fn capabilities(&self) -> Capabilities {
            test_capabilities()
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.first_entered.notify_one();
                self.release_first.notified().await;
            }
            Ok(Box::pin(futures_util::stream::iter([
                Ok(ProviderEvent::TextDelta {
                    text: format!("response-{call}"),
                }),
                Ok(ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    #[async_trait]
    impl Provider for FixtureProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self) -> Capabilities {
            test_capabilities()
        }
        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            Ok(Box::pin(futures_util::stream::iter([
                Ok(ProviderEvent::TextDelta {
                    text: "byte-identical".to_owned(),
                }),
                Ok(ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: "fixture-model".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: 10,
            temperature: None,
            thinking: ThinkingLevel::Off,
            tool_choice: ToolChoice::Auto,
        }
    }

    fn test_capabilities() -> Capabilities {
        Capabilities {
            tool_calling: true,
            vision: true,
            thinking: true,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        }
    }

    #[tokio::test]
    async fn replay_is_byte_identical_and_does_not_call_live_provider() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("rw-provider-replay-{nonce}"));
        let recorder = Recorder::new(
            Arc::new(FixtureProvider {
                name: "fixture".to_owned(),
            }),
            &directory,
            FixtureRedactor::default(),
        );
        let live = recorder
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("record stream must start: {error}"))
            .collect::<Vec<_>>()
            .await;
        let replay = ReplayProvider::load("fixture", &directory)
            .await
            .unwrap_or_else(|error| panic!("replay provider must load: {error}"))
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("replay must load: {error}"))
            .collect::<Vec<_>>()
            .await;
        let live_bytes = serde_json::to_vec(&live)
            .unwrap_or_else(|error| panic!("live events serialize: {error}"));
        let replay_bytes = serde_json::to_vec(&replay)
            .unwrap_or_else(|error| panic!("replay events serialize: {error}"));
        assert_eq!(live_bytes, replay_bytes);
        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn repeated_requests_record_and_replay_distinct_occurrences_in_order() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("rw-provider-sequence-{nonce}"));
        let recorder = Recorder::new(
            Arc::new(SequenceProvider {
                calls: AtomicUsize::new(0),
            }),
            &directory,
            FixtureRedactor::default(),
        );

        let first_live = recorder
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("first record stream must start: {error}"))
            .collect::<Vec<_>>()
            .await;
        let second_live = recorder
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("second record stream must start: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert_ne!(first_live, second_live);

        let hash = request_hash(&request())
            .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
        for occurrence in 0..=1 {
            let path = fixture_path(&directory, "sequence", &hash, occurrence);
            let bytes = tokio::fs::read(&path)
                .await
                .unwrap_or_else(|error| panic!("fixture {} must exist: {error}", path.display()));
            let fixture: RecordFixture = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                panic!("fixture {} must deserialize: {error}", path.display())
            });
            assert_eq!(fixture.occurrence, occurrence);
        }

        let replay = ReplayProvider::load("sequence", &directory)
            .await
            .unwrap_or_else(|error| panic!("replay provider must load: {error}"));
        let first_replay = replay
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("first replay must load: {error}"))
            .collect::<Vec<_>>()
            .await;
        let second_replay = replay
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("second replay must load: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(first_live, first_replay);
        assert_eq!(second_live, second_replay);

        let Err(exhausted) = replay.stream(request()).await else {
            panic!("third replay must report occurrence exhaustion");
        };
        assert_eq!(exhausted.kind, ProviderErrorKind::ReplayMiss);
        assert!(exhausted.message.contains("exhausted at occurrence 2"));

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn interrupted_stream_records_partial_occurrence_without_leaving_a_hole() {
        let directory = unique_temp_directory("interrupted-occurrence");
        let recorder = Recorder::new(
            Arc::new(InterruptibleRawProvider),
            &directory,
            FixtureRedactor::default(),
        );
        let first_delta = ProviderEvent::TextDelta {
            text: "prefix".to_owned(),
        };

        let mut first_stream = recorder
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("first record stream must start: {error}"));
        assert_eq!(first_stream.next().await, Some(Ok(first_delta.clone())));
        drop(first_stream);
        recorder
            .flush()
            .await
            .unwrap_or_else(|error| panic!("interrupted fixture must finalize: {error}"));

        let second_live = recorder
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("second record stream must start: {error}"))
            .collect::<Vec<_>>()
            .await;
        recorder
            .flush()
            .await
            .unwrap_or_else(|error| panic!("second fixture must finalize: {error}"));

        let replay = ReplayProvider::load("interruptible-raw", &directory)
            .await
            .unwrap_or_else(|error| panic!("replay provider must load: {error}"));
        let first_replay = replay
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("first replay must load: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            first_replay,
            vec![
                Ok(first_delta),
                Err(ProviderError::new(
                    ProviderErrorKind::Cancelled,
                    "provider stream was interrupted before completion",
                )),
            ]
        );

        let second_replay = replay
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("second replay must load: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(second_replay, second_live);

        let hash = request_hash(&request())
            .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
        assert!(fixture_path(&directory, "interruptible-raw", &hash, 0).is_file());
        assert!(fixture_path(&directory, "interruptible-raw", &hash, 1).is_file());

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn replay_loads_exact_restricted_capabilities_and_rejects_inconsistency() {
        let directory = unique_temp_directory("restricted-capabilities");
        let live_capabilities = RestrictedProvider.capabilities();
        let recorder = Recorder::new(
            Arc::new(RestrictedProvider),
            &directory,
            FixtureRedactor::default(),
        );
        let _ = collect(&recorder).await;

        let replay = ReplayProvider::load("restricted", &directory)
            .await
            .unwrap_or_else(|error| panic!("restricted replay must load: {error}"));
        assert_eq!(replay.capabilities(), live_capabilities);

        let hash = request_hash(&request())
            .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
        let path = fixture_path(&directory, "restricted", &hash, 0);
        let bytes = tokio::fs::read(&path)
            .await
            .unwrap_or_else(|error| panic!("restricted fixture must read: {error}"));
        let mut fixture: RecordFixture = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("restricted fixture must parse: {error}"));
        fixture.capabilities.thinking = true;
        tokio::fs::write(
            &path,
            serde_json::to_vec_pretty(&fixture)
                .unwrap_or_else(|error| panic!("tampered fixture must serialize: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("tampered fixture must write: {error}"));
        let error = ReplayProvider::load("restricted", &directory)
            .await
            .err()
            .unwrap_or_else(|| panic!("inconsistent capabilities must fail loading"));
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
        assert!(error.message.contains("inconsistent"));

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn bounded_writer_backpressure_precedes_occurrence_assignment() {
        let directory = unique_temp_directory("writer-backpressure");
        let recorder = Arc::new(Recorder::with_writer_capacity(
            Arc::new(FixtureProvider {
                name: "bounded".to_owned(),
            }),
            &directory,
            FixtureRedactor::default(),
            1,
        ));
        let first_stream = recorder
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("first bounded stream must start: {error}"));
        let mut second_start = Box::pin(recorder.stream(request()));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second_start)
                .await
                .is_err(),
            "second stream must wait for bounded writer capacity"
        );
        drop(first_stream);
        let second_live = second_start
            .await
            .unwrap_or_else(|error| panic!("second bounded stream must start: {error}"))
            .collect::<Vec<_>>()
            .await;
        recorder
            .flush()
            .await
            .unwrap_or_else(|error| panic!("bounded writer must flush: {error}"));

        let replay = ReplayProvider::load("bounded", &directory)
            .await
            .unwrap_or_else(|error| panic!("bounded replay must load: {error}"));
        let first_replay = replay
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("first bounded replay must start: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            first_replay.as_slice(),
            [Err(ProviderError {
                kind: ProviderErrorKind::Cancelled,
                ..
            })]
        ));
        let second_replay = replay
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("second bounded replay must start: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(second_replay, second_live);

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn writer_error_is_reported_by_stream_and_once_by_flush_barrier() {
        let root = unique_temp_directory("writer-error");
        tokio::fs::write(&root, b"not a directory")
            .await
            .unwrap_or_else(|error| panic!("writer-error sentinel must write: {error}"));
        let recorder = Recorder::new(
            Arc::new(FixtureProvider {
                name: "writer-error".to_owned(),
            }),
            &root,
            FixtureRedactor::default(),
        );
        let items = collect(&recorder).await;
        assert!(matches!(
            items.last(),
            Some(Err(ProviderError {
                kind: ProviderErrorKind::Protocol,
                ..
            }))
        ));
        let first_error = recorder
            .flush()
            .await
            .err()
            .unwrap_or_else(|| panic!("first flush must surface the writer error"));
        assert_eq!(first_error.kind, ProviderErrorKind::Protocol);
        recorder
            .flush()
            .await
            .unwrap_or_else(|error| panic!("second flush must clear prior error: {error}"));

        let _ = tokio::fs::remove_file(root).await;
    }

    #[tokio::test]
    async fn providers_sharing_a_directory_do_not_collide() {
        let directory = unique_temp_directory("provider-scope");
        let alpha = Recorder::new(
            Arc::new(FixtureProvider {
                name: "alpha".to_owned(),
            }),
            &directory,
            FixtureRedactor::default(),
        );
        let beta = Recorder::new(
            Arc::new(FixtureProvider {
                name: "beta".to_owned(),
            }),
            &directory,
            FixtureRedactor::default(),
        );

        let alpha_live = collect(&alpha).await;
        let beta_live = collect(&beta).await;
        let hash = request_hash(&request())
            .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
        let alpha_path = fixture_path(&directory, "alpha", &hash, 0);
        let beta_path = fixture_path(&directory, "beta", &hash, 0);
        assert_ne!(alpha_path, beta_path);
        assert!(alpha_path.is_file());
        assert!(beta_path.is_file());

        let alpha_replay = collect(
            &ReplayProvider::load("alpha", &directory)
                .await
                .unwrap_or_else(|error| panic!("alpha replay must load: {error}")),
        )
        .await;
        let beta_replay = collect(
            &ReplayProvider::load("beta", &directory)
                .await
                .unwrap_or_else(|error| panic!("beta replay must load: {error}")),
        )
        .await;
        assert_eq!(alpha_live, alpha_replay);
        assert_eq!(beta_live, beta_replay);

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn replay_rejects_a_fixture_claiming_another_provider() {
        let directory = unique_temp_directory("provider-validation");
        let alpha = Recorder::new(
            Arc::new(FixtureProvider {
                name: "alpha".to_owned(),
            }),
            &directory,
            FixtureRedactor::default(),
        );
        let _ = collect(&alpha).await;
        let hash = request_hash(&request())
            .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
        let alpha_path = fixture_path(&directory, "alpha", &hash, 0);
        let beta_path = fixture_path(&directory, "beta", &hash, 0);
        tokio::fs::copy(alpha_path, beta_path)
            .await
            .unwrap_or_else(|error| panic!("fixture copy must succeed: {error}"));

        let Err(error) = ReplayProvider::load("beta", &directory).await else {
            panic!("provider mismatch must be rejected");
        };
        assert_eq!(error.kind, ProviderErrorKind::ReplayMiss);
        assert!(error.message.contains("provider"));

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn provider_start_error_is_recorded_and_replayed_as_start_error() {
        let directory = unique_temp_directory("start-error");
        let recorder = Recorder::new(
            Arc::new(StartErrorProvider),
            &directory,
            FixtureRedactor::default(),
        );

        let Err(live_error) = recorder.stream(request()).await else {
            panic!("live provider start must fail");
        };
        let replay = ReplayProvider::load("start-error", &directory)
            .await
            .unwrap_or_else(|error| panic!("start-error replay must load: {error}"));
        let Err(replay_error) = replay.stream(request()).await else {
            panic!("replayed provider start must fail");
        };
        assert_eq!(live_error, replay_error);
        assert!(replay_error.is_retryable());
        assert_eq!(replay_error.retry_after_ms, Some(1_250));

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn metadata_discovery_error_is_recorded_without_an_occurrence_hole() {
        let directory = unique_temp_directory("metadata-start-error");
        let recorder = Recorder::new(
            Arc::new(MetadataErrorProvider),
            &directory,
            FixtureRedactor::default(),
        );

        let Err(live_error) = recorder.stream(request()).await else {
            panic!("metadata discovery must fail");
        };
        let replay = ReplayProvider::load("metadata-error", &directory)
            .await
            .unwrap_or_else(|error| panic!("metadata-error replay must load: {error}"));
        let Err(replay_error) = replay.stream(request()).await else {
            panic!("metadata discovery start error must replay");
        };
        assert_eq!(live_error, replay_error);
        assert_eq!(replay_error.kind, ProviderErrorKind::Server);
        let hash = request_hash(&request())
            .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
        assert!(fixture_path(&directory, "metadata-error", &hash, 0).is_file());
        assert!(!fixture_path(&directory, "metadata-error", &hash, 1).is_file());

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn transient_metadata_failure_then_success_upgrades_manifest_and_replays_both() {
        let directory = unique_temp_directory("metadata-evolution");
        let recorder = Recorder::new(
            Arc::new(FlakyMetadataProvider {
                metadata_calls: AtomicUsize::new(0),
            }),
            &directory,
            FixtureRedactor::default(),
        );

        let Err(first_live) = recorder.stream(request()).await else {
            panic!("first metadata discovery must fail");
        };
        let second_live = collect(&recorder).await;
        assert!(matches!(
            second_live.as_slice(),
            [
                Ok(ProviderEvent::MessageStart { .. }),
                Ok(ProviderEvent::TextDelta { text }),
                Ok(ProviderEvent::Usage { .. }),
                Ok(ProviderEvent::Finished { .. })
            ] if text == "resolved"
        ));

        let replay = ReplayProvider::load("flaky-metadata", &directory)
            .await
            .unwrap_or_else(|error| panic!("evolved replay must load: {error}"));
        let metadata = replay
            .model_metadata()
            .await
            .unwrap_or_else(|error| panic!("replay metadata must resolve: {error}"))
            .unwrap_or_else(|| panic!("resolved metadata must persist"));
        assert_eq!(
            metadata.capabilities.wire_mode,
            WireMode::GitHubCopilotResponses
        );
        let Err(first_replay) = replay.stream(request()).await else {
            panic!("first replay occurrence must retain discovery failure");
        };
        assert_eq!(first_live, first_replay);
        let second_replay = collect(&replay).await;
        assert_eq!(second_live, second_replay);

        let hash = request_hash(&request())
            .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
        let first_bytes = tokio::fs::read(fixture_path(&directory, "flaky-metadata", &hash, 0))
            .await
            .unwrap_or_else(|error| panic!("first fixture must read: {error}"));
        let first: RecordFixture = serde_json::from_slice(&first_bytes)
            .unwrap_or_else(|error| panic!("first fixture must parse: {error}"));
        let second_bytes = tokio::fs::read(fixture_path(&directory, "flaky-metadata", &hash, 1))
            .await
            .unwrap_or_else(|error| panic!("second fixture must read: {error}"));
        let second: RecordFixture = serde_json::from_slice(&second_bytes)
            .unwrap_or_else(|error| panic!("second fixture must parse: {error}"));
        assert_eq!(first.model_metadata, None);
        assert_eq!(first.wire_mode, WireMode::GitHubCopilot);
        assert!(second.model_metadata.is_some());
        assert_eq!(second.wire_mode, WireMode::GitHubCopilotResponses);

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn legacy_generic_copilot_fixture_replays_normalized_items_without_guessing() {
        let directory = unique_temp_directory("legacy-copilot-wire");
        let recorder = Recorder::new(
            Arc::new(LegacyCopilotRawProvider),
            &directory,
            FixtureRedactor::default(),
        );
        let live = collect(&recorder).await;
        let replay = ReplayProvider::load("legacy-copilot", &directory)
            .await
            .unwrap_or_else(|error| panic!("legacy replay must load: {error}"));
        let replayed = collect(&replay).await;
        assert_eq!(live, replayed);

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn resolved_exact_dialect_replays_empty_raw_start_error() {
        let directory = unique_temp_directory("resolved-empty-raw");
        let recorder = Recorder::new(
            Arc::new(ResolvedMetadataStartErrorProvider),
            &directory,
            FixtureRedactor::default(),
        );
        let Err(live_error) = recorder.stream(request()).await else {
            panic!("resolved provider start must fail");
        };
        let replay = ReplayProvider::load("resolved-metadata-start-error", &directory)
            .await
            .unwrap_or_else(|error| panic!("resolved replay must load: {error}"));
        assert_eq!(
            replay.capabilities().wire_mode,
            WireMode::GitHubCopilotResponses
        );
        let Err(replay_error) = replay.stream(request()).await else {
            panic!("empty-raw start error must replay");
        };
        assert_eq!(live_error, replay_error);

        let hash = request_hash(&request())
            .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
        let bytes = tokio::fs::read(fixture_path(
            &directory,
            "resolved-metadata-start-error",
            &hash,
            0,
        ))
        .await
        .unwrap_or_else(|error| panic!("fixture must read: {error}"));
        let fixture: RecordFixture = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("fixture must parse: {error}"));
        assert_eq!(fixture.wire_mode, WireMode::GitHubCopilotResponses);
        assert!(fixture.raw_sse.is_empty());

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn raw_frame_prefix_replays_the_recorded_transport_error() {
        let directory = unique_temp_directory("raw-prefix");
        let recorder = Recorder::new(
            Arc::new(RawPrefixProvider),
            &directory,
            FixtureRedactor::default(),
        );

        let live = collect(&recorder).await;
        let replay = collect(
            &ReplayProvider::load("raw-prefix", &directory)
                .await
                .unwrap_or_else(|error| panic!("raw-prefix replay must load: {error}")),
        )
        .await;
        assert_eq!(live, replay);
        assert!(matches!(
            replay.last(),
            Some(Err(ProviderError {
                kind: ProviderErrorKind::Network,
                message,
                ..
            })) if message == "fixture connection reset"
        ));

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn concurrent_start_completion_order_does_not_renumber_occurrences() {
        let directory = unique_temp_directory("start-order");
        let first_entered = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let recorder = Arc::new(Recorder::new(
            Arc::new(DelayedStartProvider {
                calls: AtomicUsize::new(0),
                first_entered: Arc::clone(&first_entered),
                release_first: Arc::clone(&release_first),
            }),
            &directory,
            FixtureRedactor::default(),
        ));

        let first_task = tokio::spawn({
            let recorder = Arc::clone(&recorder);
            async move { collect(recorder.as_ref()).await }
        });
        first_entered.notified().await;
        let second_task = tokio::spawn({
            let recorder = Arc::clone(&recorder);
            async move { collect(recorder.as_ref()).await }
        });
        let second_live = second_task
            .await
            .unwrap_or_else(|error| panic!("second record task must join: {error}"));
        release_first.notify_one();
        let first_live = first_task
            .await
            .unwrap_or_else(|error| panic!("first record task must join: {error}"));
        assert!(matches!(
            first_live.first(),
            Some(Ok(ProviderEvent::TextDelta { text })) if text == "response-0"
        ));
        assert!(matches!(
            second_live.first(),
            Some(Ok(ProviderEvent::TextDelta { text })) if text == "response-1"
        ));

        let replay = ReplayProvider::load("delayed", &directory)
            .await
            .unwrap_or_else(|error| panic!("delayed replay must load: {error}"));
        let first_replay = collect(&replay).await;
        let second_replay = collect(&replay).await;
        assert_eq!(first_replay, first_live);
        assert_eq!(second_replay, second_live);

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    fn unique_temp_directory(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("rw-provider-{label}-{nonce}"))
    }

    async fn collect(provider: &dyn Provider) -> Vec<Result<ProviderEvent, ProviderError>> {
        provider
            .stream(request())
            .await
            .unwrap_or_else(|error| panic!("fixture stream must start: {error}"))
            .collect()
            .await
    }
}
