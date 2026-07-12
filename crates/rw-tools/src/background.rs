use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use rw_types::{SessionId, ToolCapability, ToolOutputStream};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

use crate::bash::{CommandExecutor, CommandFixtureRedactor, CommandRequest};
use crate::registry::{
    CancellationToken, CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError,
    ToolOutputChunk, ToolOutputSink, ToolResult, WorkspaceBinding, input_schema, parse_input,
};

/// Hard resource bounds for one session-owned background process manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackgroundProcessLimits {
    pub max_running_per_session: usize,
    pub max_completed_per_session: usize,
    pub max_retained_output_bytes: usize,
    pub max_output_query_bytes: usize,
    pub shutdown_timeout: Duration,
}

impl Default for BackgroundProcessLimits {
    fn default() -> Self {
        Self {
            max_running_per_session: 16,
            max_completed_per_session: 64,
            max_retained_output_bytes: 256 * 1024,
            max_output_query_bytes: 64 * 1024,
            shutdown_timeout: Duration::from_secs(5),
        }
    }
}

/// Stable model-visible state for one managed process.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum BackgroundProcessStatus {
    Running,
    Exited { exit_code: i32 },
    Killed,
    Failed { message: String },
}

impl BackgroundProcessStatus {
    fn running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackgroundProcessSnapshot {
    pub process_id: String,
    pub status: BackgroundProcessStatus,
    pub retained_output_bytes: usize,
    pub dropped_output_bytes: u64,
}

#[derive(Default)]
struct OutputTail {
    chunks: VecDeque<ToolOutputChunk>,
    retained_bytes: usize,
    dropped_bytes: u64,
}

impl OutputTail {
    fn push(&mut self, mut chunk: ToolOutputChunk, limit: usize) {
        if limit == 0 {
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(chunk.content.len()).unwrap_or(u64::MAX));
            return;
        }
        if chunk.content.len() > limit {
            let original = chunk.content.len();
            let retained = tail_utf8(&chunk.content, limit).to_owned();
            chunk.content = retained;
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(original - chunk.content.len()).unwrap_or(u64::MAX));
        }
        self.retained_bytes = self.retained_bytes.saturating_add(chunk.content.len());
        self.chunks.push_back(chunk);
        while self.retained_bytes > limit {
            let Some(front) = self.chunks.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(front.content.len());
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(front.content.len()).unwrap_or(u64::MAX));
        }
    }

    fn tail(&self, limit: usize) -> Vec<ToolOutputChunk> {
        let mut remaining = limit;
        let mut reversed = Vec::new();
        for chunk in self.chunks.iter().rev() {
            if remaining == 0 {
                break;
            }
            let content = tail_utf8(&chunk.content, remaining);
            remaining = remaining.saturating_sub(content.len());
            reversed.push(ToolOutputChunk {
                stream: chunk.stream.clone(),
                content: content.to_owned(),
            });
        }
        reversed.reverse();
        reversed
    }
}

fn tail_utf8(value: &str, limit: usize) -> &str {
    if value.len() <= limit {
        return value;
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

struct ProcessState {
    status: BackgroundProcessStatus,
    output: OutputTail,
}

struct ProcessEntry {
    id: String,
    cancellation: CancellationToken,
    state: Mutex<ProcessState>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ProcessEntry {
    fn snapshot(&self) -> BackgroundProcessSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        BackgroundProcessSnapshot {
            process_id: self.id.clone(),
            status: state.status.clone(),
            retained_output_bytes: state.output.retained_bytes,
            dropped_output_bytes: state.output.dropped_bytes,
        }
    }
}

#[derive(Default)]
struct SessionProcesses {
    processes: BTreeMap<String, Arc<ProcessEntry>>,
    launch_order: VecDeque<String>,
    next_id: u64,
}

struct ManagerInner {
    redactor: Arc<dyn CommandFixtureRedactor>,
    limits: BackgroundProcessLimits,
    sessions: Mutex<BTreeMap<String, SessionProcesses>>,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        let sessions = self
            .sessions
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for session in sessions.values() {
            for process in session.processes.values() {
                process.cancellation.cancel();
            }
        }
    }
}

/// Session-owned supervisor for shell commands intentionally left running.
#[derive(Clone)]
pub struct BackgroundProcessManager {
    inner: Arc<ManagerInner>,
}

impl BackgroundProcessManager {
    #[must_use]
    pub fn new(redactor: Arc<dyn CommandFixtureRedactor>, limits: BackgroundProcessLimits) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                redactor,
                limits,
                sessions: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Starts a command after the ordinary `bash` capability approval has run.
    ///
    /// # Errors
    ///
    /// Returns an error when the session process limit has been reached.
    pub fn start(
        &self,
        executor: Arc<dyn CommandExecutor>,
        session_id: &SessionId,
        request: CommandRequest,
    ) -> Result<BackgroundProcessSnapshot, ToolError> {
        if self.inner.redactor.max_secret_bytes() > MAX_REDACTION_CARRY_BYTES + 1 {
            return Err(ToolError::Command(
                "background output redaction pattern exceeds the supported streaming bound"
                    .to_owned(),
            ));
        }
        if !executor.supports_background() {
            return Err(ToolError::Command(
                "background execution is unavailable in command record/replay mode".to_owned(),
            ));
        }
        let entry = {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let session = sessions.entry(session_id.0.clone()).or_default();
            prune_completed(session, self.inner.limits.max_completed_per_session);
            let running = session
                .processes
                .values()
                .filter(|process| process.snapshot().status.running())
                .count();
            if running >= self.inner.limits.max_running_per_session {
                return Err(ToolError::Command(format!(
                    "background process limit reached ({})",
                    self.inner.limits.max_running_per_session
                )));
            }
            session.next_id = session.next_id.saturating_add(1);
            let process_id = format!("bg-{:016x}", session.next_id);
            let entry = Arc::new(ProcessEntry {
                id: process_id.clone(),
                cancellation: CancellationToken::default(),
                state: Mutex::new(ProcessState {
                    status: BackgroundProcessStatus::Running,
                    output: OutputTail::default(),
                }),
                task: Mutex::new(None),
            });
            session.processes.insert(process_id, Arc::clone(&entry));
            session.launch_order.push_back(entry.id.clone());
            entry
        };

        let redactor = Arc::clone(&self.inner.redactor);
        let limits = self.inner.limits;
        let weak_inner = Arc::downgrade(&self.inner);
        let weak_entry = Arc::downgrade(&entry);
        let owned_session = session_id.0.clone();
        let cancellation = entry.cancellation.clone();
        let background_sink = Arc::new(BackgroundOutputSink {
            entry: Arc::downgrade(&entry),
            redactor: Arc::clone(&redactor),
            limit: limits.max_retained_output_bytes,
            pending: Mutex::new(PendingStreams::default()),
        });
        let sink: Arc<dyn ToolOutputSink> = background_sink.clone();
        let task = tokio::spawn(async move {
            let result = executor.run(request, cancellation.clone(), sink).await;
            background_sink.finish();
            let status = match result {
                Ok(outcome) => BackgroundProcessStatus::Exited {
                    exit_code: outcome.exit_code,
                },
                Err(ToolError::Cancelled) if cancellation.is_cancelled() => {
                    BackgroundProcessStatus::Killed
                }
                Err(error) => BackgroundProcessStatus::Failed {
                    message: redactor.redact(&error.to_string()),
                },
            };
            if let Some(entry) = weak_entry.upgrade() {
                entry
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .status = status;
            }
            record_completion(&weak_inner, &owned_session);
        });
        *entry
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
        Ok(entry.snapshot())
    }

    /// Returns one or every process visible to the exact session.
    ///
    /// # Errors
    ///
    /// Returns an error when a requested process is not owned by the session.
    pub fn status(
        &self,
        session_id: &SessionId,
        process_id: Option<&str>,
    ) -> Result<Vec<BackgroundProcessSnapshot>, ToolError> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(session) = sessions.get(&session_id.0) else {
            return Ok(Vec::new());
        };
        if let Some(process_id) = process_id {
            let process = session.processes.get(process_id).ok_or_else(|| {
                ToolError::InvalidInput("unknown background process for this session".to_owned())
            })?;
            return Ok(vec![process.snapshot()]);
        }
        Ok(session
            .processes
            .values()
            .map(|process| process.snapshot())
            .collect())
    }

    #[must_use]
    pub fn has_running(&self, session_id: &SessionId) -> bool {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id.0)
            .is_some_and(|session| {
                session
                    .processes
                    .values()
                    .any(|process| process.snapshot().status.running())
            })
    }

    /// Returns a bounded retained output tail for one owned process.
    ///
    /// # Errors
    ///
    /// Returns an error when the process is not owned by the session.
    pub fn output(
        &self,
        session_id: &SessionId,
        process_id: &str,
        requested_limit: Option<usize>,
    ) -> Result<(BackgroundProcessSnapshot, Vec<ToolOutputChunk>), ToolError> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let process = sessions
            .get(&session_id.0)
            .and_then(|session| session.processes.get(process_id))
            .ok_or_else(|| {
                ToolError::InvalidInput("unknown background process for this session".to_owned())
            })?;
        let state = process
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let limit = requested_limit
            .unwrap_or(self.inner.limits.max_output_query_bytes)
            .min(self.inner.limits.max_output_query_bytes);
        let chunks = state.output.tail(limit);
        let snapshot = BackgroundProcessSnapshot {
            process_id: process.id.clone(),
            status: state.status.clone(),
            retained_output_bytes: state.output.retained_bytes,
            dropped_output_bytes: state.output.dropped_bytes,
        };
        Ok((snapshot, chunks))
    }

    /// Cancels, kills, and reaps one owned process group.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown process or a bounded reap failure.
    pub async fn kill(
        &self,
        session_id: &SessionId,
        process_id: &str,
    ) -> Result<BackgroundProcessSnapshot, ToolError> {
        let process = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sessions
                .get(&session_id.0)
                .and_then(|session| session.processes.get(process_id))
                .cloned()
                .ok_or_else(|| {
                    ToolError::InvalidInput(
                        "unknown background process for this session".to_owned(),
                    )
                })?
        };
        process.cancellation.cancel();
        await_process(&process, self.inner.limits.shutdown_timeout).await?;
        Ok(process.snapshot())
    }

    /// Cancels, kills, and reaps every process owned by one ending session.
    ///
    /// # Errors
    ///
    /// Returns an error when a process cannot be reaped before the deadline.
    pub async fn shutdown_session(&self, session_id: &SessionId) -> Result<(), ToolError> {
        let processes = {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sessions
                .remove(&session_id.0)
                .map(|session| session.processes.into_values().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        for process in &processes {
            process.cancellation.cancel();
        }
        let mut first_error = None;
        for process in processes {
            if let Err(error) = await_process(&process, self.inner.limits.shutdown_timeout).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn record_completion(inner: &Weak<ManagerInner>, session_id: &str) {
    let Some(inner) = inner.upgrade() else {
        return;
    };
    let mut sessions = inner
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(session) = sessions.get_mut(session_id) else {
        return;
    };
    prune_completed(session, inner.limits.max_completed_per_session);
}

fn prune_completed(session: &mut SessionProcesses, limit: usize) {
    loop {
        let completed = session
            .launch_order
            .iter()
            .filter(|process_id| {
                session
                    .processes
                    .get(*process_id)
                    .is_some_and(|process| !process.snapshot().status.running())
            })
            .count();
        if completed <= limit {
            break;
        }
        let Some(index) = session.launch_order.iter().position(|process_id| {
            session
                .processes
                .get(process_id)
                .is_some_and(|process| !process.snapshot().status.running())
        }) else {
            break;
        };
        let Some(process_id) = session.launch_order.remove(index) else {
            break;
        };
        session.processes.remove(&process_id);
    }
}

async fn await_process(entry: &Arc<ProcessEntry>, wait: Duration) -> Result<(), ToolError> {
    let task = entry
        .task
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(mut task) = task else {
        return Ok(());
    };
    match timeout(wait, &mut task).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(ToolError::Command(format!(
            "background process supervisor task failed: {error}"
        ))),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err(ToolError::Command(
                "background process did not reap before the shutdown deadline".to_owned(),
            ))
        }
    }
}

struct BackgroundOutputSink {
    entry: Weak<ProcessEntry>,
    redactor: Arc<dyn CommandFixtureRedactor>,
    limit: usize,
    pending: Mutex<PendingStreams>,
}

const MAX_REDACTION_CARRY_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct PendingStreams {
    stdout: String,
    stderr: String,
}

impl BackgroundOutputSink {
    fn retain_redacted(&self, stream: ToolOutputStream, content: String) {
        if content.is_empty() {
            return;
        }
        if let Some(entry) = self.entry.upgrade() {
            entry
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .output
                .push(ToolOutputChunk { stream, content }, self.limit);
        }
    }

    fn finish(&self) {
        let (stdout, stderr) = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                std::mem::take(&mut pending.stdout),
                std::mem::take(&mut pending.stderr),
            )
        };
        self.retain_redacted(ToolOutputStream::Stdout, self.redactor.redact(&stdout));
        self.retain_redacted(ToolOutputStream::Stderr, self.redactor.redact(&stderr));
    }
}

#[async_trait]
impl ToolOutputSink for BackgroundOutputSink {
    async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
        if self.entry.upgrade().is_none() {
            return Err(ToolError::Cancelled);
        }
        let flushed = {
            let mut streams = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let pending = match &chunk.stream {
                ToolOutputStream::Stdout => &mut streams.stdout,
                ToolOutputStream::Stderr => &mut streams.stderr,
            };
            pending.push_str(&chunk.content);
            let carry = self
                .redactor
                .max_secret_bytes()
                .saturating_sub(1)
                .min(MAX_REDACTION_CARRY_BYTES);
            if pending.len() <= carry {
                None
            } else {
                let redacted = self.redactor.redact(pending);
                let tail_start = utf8_tail_start(pending, carry);
                let tail = pending[tail_start..].to_owned();
                let redacted_tail = self.redactor.redact(&tail);
                if redacted.ends_with(&redacted_tail) {
                    let prefix_len = redacted.len().saturating_sub(redacted_tail.len());
                    let prefix = redacted[..prefix_len].to_owned();
                    *pending = tail;
                    Some(prefix)
                } else {
                    if pending.len() > MAX_REDACTION_CARRY_BYTES.saturating_mul(2) {
                        return Err(ToolError::Output(
                            "stream redaction could not establish a bounded safe boundary"
                                .to_owned(),
                        ));
                    }
                    None
                }
            }
        };
        if let Some(content) = flushed {
            self.retain_redacted(chunk.stream.clone(), content);
        }
        Ok(())
    }
}

fn utf8_tail_start(value: &str, limit: usize) -> usize {
    if value.len() <= limit {
        return 0;
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackgroundStatusInput {
    pub process_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackgroundOutputInput {
    pub process_id: String,
    pub tail_bytes: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackgroundKillInput {
    pub process_id: String,
}

#[derive(Clone)]
pub struct BackgroundStatusTool {
    manager: Arc<BackgroundProcessManager>,
}

impl BackgroundStatusTool {
    #[must_use]
    pub fn new(manager: Arc<BackgroundProcessManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for BackgroundStatusTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "background_status".to_owned(),
            description: "List session-owned background processes or inspect one process."
                .to_owned(),
            input_schema: input_schema::<BackgroundStatusInput>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: BackgroundStatusInput = parse_input(input)?;
        let session_id = require_session(context)?;
        let processes = self
            .manager
            .status(session_id, input.process_id.as_deref())?;
        Ok(ToolResult::new(
            serde_json::to_string_pretty(&processes)
                .map_err(|error| ToolError::Output(error.to_string()))?,
            json!({ "processes": processes }),
        ))
    }
}

#[derive(Clone)]
pub struct BackgroundOutputTool {
    manager: Arc<BackgroundProcessManager>,
}

impl BackgroundOutputTool {
    #[must_use]
    pub fn new(manager: Arc<BackgroundProcessManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for BackgroundOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "background_output".to_owned(),
            description: "Stream a bounded tail of one session-owned background process."
                .to_owned(),
            input_schema: input_schema::<BackgroundOutputInput>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: BackgroundOutputInput = parse_input(input)?;
        let session_id = require_session(context)?;
        let (process, chunks) =
            self.manager
                .output(session_id, &input.process_id, input.tail_bytes)?;
        for chunk in &chunks {
            context.output.emit(chunk.clone()).await?;
        }
        let mut rendered = String::new();
        for chunk in &chunks {
            let stream = match &chunk.stream {
                ToolOutputStream::Stdout => "stdout",
                ToolOutputStream::Stderr => "stderr",
            };
            write!(rendered, "[{stream}] {}", chunk.content)
                .map_err(|error| ToolError::Output(error.to_string()))?;
        }
        Ok(ToolResult::new(
            rendered,
            json!({ "process": process, "chunks": chunks }),
        ))
    }
}

#[derive(Clone)]
pub struct BackgroundKillTool {
    manager: Arc<BackgroundProcessManager>,
}

impl BackgroundKillTool {
    #[must_use]
    pub fn new(manager: Arc<BackgroundProcessManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for BackgroundKillTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "background_kill".to_owned(),
            description: "Kill and reap one session-owned background process group.".to_owned(),
            input_schema: input_schema::<BackgroundKillInput>(),
            capabilities: CapabilityManifest::new([ToolCapability::Execute]),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: BackgroundKillInput = parse_input(input)?;
        let session_id = require_session(context)?;
        let process = self.manager.kill(session_id, &input.process_id).await?;
        Ok(ToolResult::new(
            format!("{}: {:?}", process.process_id, process.status),
            json!({ "process": process }),
        ))
    }
}

fn require_session(context: &ToolContext) -> Result<&SessionId, ToolError> {
    context.session_id().ok_or_else(|| {
        ToolError::Command("background process tools require an actor-owned session".to_owned())
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::tempdir;

    use super::*;
    use crate::{
        BashSandboxMode, BashTool, IdentityCommandFixtureRedactor, RecordingCommandExecutor,
        ReplayCommandExecutor, TokioCommandExecutor, ToolLimits, ToolRegistry,
    };

    #[derive(Default)]
    struct BlockingFixture {
        starts: AtomicUsize,
    }

    #[async_trait]
    impl CommandExecutor for BlockingFixture {
        fn supports_background(&self) -> bool {
            true
        }

        async fn run(
            &self,
            _request: CommandRequest,
            cancellation: CancellationToken,
            output: Arc<dyn ToolOutputSink>,
        ) -> Result<crate::CommandOutcome, ToolError> {
            self.starts.fetch_add(1, Ordering::Release);
            output
                .emit(ToolOutputChunk {
                    stream: ToolOutputStream::Stdout,
                    content: format!("{} canary-secret {}", "x".repeat(128), "z".repeat(32)),
                })
                .await?;
            cancellation.cancelled().await;
            Err(ToolError::Cancelled)
        }
    }

    struct CanaryRedactor;

    impl CommandFixtureRedactor for CanaryRedactor {
        fn redact(&self, value: &str) -> String {
            value.replace("canary-secret", "[REDACTED]")
        }

        fn max_secret_bytes(&self) -> usize {
            "canary-secret".len()
        }
    }

    struct SplitSecretFixture {
        emitted: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CommandExecutor for SplitSecretFixture {
        fn supports_background(&self) -> bool {
            true
        }

        async fn run(
            &self,
            _request: CommandRequest,
            cancellation: CancellationToken,
            output: Arc<dyn ToolOutputSink>,
        ) -> Result<crate::CommandOutcome, ToolError> {
            for content in ["prefix canary-", "secret suffix-with-enough-safe-tail"] {
                output
                    .emit(ToolOutputChunk {
                        stream: ToolOutputStream::Stdout,
                        content: content.to_owned(),
                    })
                    .await?;
            }
            self.emitted.store(1, Ordering::Release);
            cancellation.cancelled().await;
            Err(ToolError::Cancelled)
        }
    }

    fn request(root: &std::path::Path, command: &str) -> CommandRequest {
        CommandRequest {
            command: command.to_owned(),
            cwd: root.to_path_buf(),
            env: BTreeMap::new(),
            network_domains: Vec::new(),
            sandbox: BashSandboxMode::Unsandboxed,
        }
    }

    async fn wait_for_status(
        manager: &BackgroundProcessManager,
        session: &SessionId,
        process_id: &str,
        expected_running: bool,
    ) -> BackgroundProcessSnapshot {
        for _ in 0..200 {
            let snapshot = manager
                .status(session, Some(process_id))
                .expect("status")
                .remove(0);
            if snapshot.status.running() == expected_running {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
        panic!("process state did not converge")
    }

    #[tokio::test]
    async fn retained_output_is_redacted_bounded_and_session_isolated() {
        let root = tempdir().expect("root");
        let executor = Arc::new(BlockingFixture::default());
        let manager = BackgroundProcessManager::new(
            Arc::new(CanaryRedactor),
            BackgroundProcessLimits {
                max_retained_output_bytes: 64,
                max_output_query_bytes: 64,
                ..BackgroundProcessLimits::default()
            },
        );
        let first = SessionId("first".to_owned());
        let second = SessionId("second".to_owned());
        let started = manager
            .start(executor.clone(), &first, request(root.path(), "fixture"))
            .expect("start");
        while executor.starts.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        let (snapshot, chunks) = manager
            .output(&first, &started.process_id, None)
            .expect("output");
        let rendered = chunks
            .iter()
            .map(|chunk| chunk.content.as_str())
            .collect::<String>();
        assert!(snapshot.retained_output_bytes <= 64);
        assert!(snapshot.dropped_output_bytes > 0);
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("canary-secret"));
        assert!(
            manager
                .status(&second, Some(&started.process_id))
                .expect("isolated status")
                .is_empty(),
            "another session must not discover or inspect the process id"
        );
        manager.shutdown_session(&first).await.expect("shutdown");
        assert!(manager.status(&first, None).expect("empty").is_empty());
    }

    #[tokio::test]
    async fn redaction_carry_catches_a_secret_split_across_output_chunks() {
        let root = tempdir().expect("root");
        let manager = BackgroundProcessManager::new(
            Arc::new(CanaryRedactor),
            BackgroundProcessLimits::default(),
        );
        let session = SessionId("split-secret".to_owned());
        let emitted = Arc::new(AtomicUsize::new(0));
        let started = manager
            .start(
                Arc::new(SplitSecretFixture {
                    emitted: Arc::clone(&emitted),
                }),
                &session,
                request(root.path(), "fixture"),
            )
            .expect("start");
        while emitted.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        let (_, chunks) = manager
            .output(&session, &started.process_id, None)
            .expect("output");
        let rendered = chunks
            .iter()
            .map(|chunk| chunk.content.as_str())
            .collect::<String>();
        assert!(rendered.contains("prefix [REDACTED]"));
        assert!(!rendered.contains("canary-secret"));
        assert!(manager.has_running(&session));
        manager.shutdown_session(&session).await.expect("shutdown");
    }

    #[tokio::test]
    async fn kill_is_bounded_and_reaps_the_owned_task() {
        let root = tempdir().expect("root");
        let executor = Arc::new(BlockingFixture::default());
        let manager = BackgroundProcessManager::new(
            Arc::new(IdentityCommandFixtureRedactor),
            BackgroundProcessLimits::default(),
        );
        let session = SessionId("kill".to_owned());
        let started = manager
            .start(executor, &session, request(root.path(), "fixture"))
            .expect("start");
        let killed = manager
            .kill(&session, &started.process_id)
            .await
            .expect("kill");
        assert_eq!(killed.status, BackgroundProcessStatus::Killed);
    }

    #[tokio::test]
    async fn running_limit_is_per_session_and_ids_are_replay_stable() {
        let root = tempdir().expect("root");
        let executor = Arc::new(BlockingFixture::default());
        let manager = BackgroundProcessManager::new(
            Arc::new(IdentityCommandFixtureRedactor),
            BackgroundProcessLimits {
                max_running_per_session: 1,
                ..BackgroundProcessLimits::default()
            },
        );
        let first = SessionId("limit-a".to_owned());
        let second = SessionId("limit-b".to_owned());
        let first_process = manager
            .start(executor.clone(), &first, request(root.path(), "fixture"))
            .expect("first start");
        assert!(
            manager
                .start(executor.clone(), &first, request(root.path(), "fixture"))
                .is_err()
        );
        let second_process = manager
            .start(executor, &second, request(root.path(), "fixture"))
            .expect("second session start");
        assert_eq!(first_process.process_id, second_process.process_id);
        manager
            .shutdown_session(&first)
            .await
            .expect("first shutdown");
        manager
            .shutdown_session(&second)
            .await
            .expect("second shutdown");
    }

    #[tokio::test]
    async fn typed_bash_background_start_is_cleaned_by_registry_session_end() {
        let root = tempdir().expect("root");
        let executor = Arc::new(BlockingFixture::default());
        let manager = Arc::new(BackgroundProcessManager::new(
            Arc::new(IdentityCommandFixtureRedactor),
            BackgroundProcessLimits::default(),
        ));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(
                BashTool::new(executor.clone(), ToolLimits::default())
                    .with_background_manager(manager.clone()),
            ))
            .expect("register bash");
        registry
            .register(Arc::new(BackgroundStatusTool::new(manager.clone())))
            .expect("register status");
        let session = SessionId("typed-bash".to_owned());
        let context = ToolContext::new(root.path())
            .expect("context")
            .with_session_id(session.clone());
        let result = registry
            .resolve("bash")
            .expect("bash")
            .execute(
                &context,
                json!({
                    "command": "fixture",
                    "sandbox": "sandboxed",
                    "run_in_background": true
                }),
            )
            .await
            .expect("background bash");
        let process_id = result
            .data
            .pointer("/background_process/process_id")
            .and_then(Value::as_str)
            .expect("process id")
            .to_owned();
        assert!(process_id.starts_with("bg-"));
        let filtered = registry.subset(["background_status"]).expect("subset");
        assert!(filtered.session_activity(&session).is_some());
        filtered.end_session(&session).await.expect("session end");
        assert!(manager.status(&session, None).expect("status").is_empty());
    }

    #[derive(Default)]
    struct CompletingFixture {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl CommandExecutor for CompletingFixture {
        fn supports_background(&self) -> bool {
            true
        }

        async fn run(
            &self,
            _request: CommandRequest,
            _cancellation: CancellationToken,
            output: Arc<dyn ToolOutputSink>,
        ) -> Result<crate::CommandOutcome, ToolError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            output
                .emit(ToolOutputChunk {
                    stream: ToolOutputStream::Stdout,
                    content: "recorded output".to_owned(),
                })
                .await?;
            Ok(crate::CommandOutcome { exit_code: 7 })
        }
    }

    #[tokio::test]
    async fn completed_retention_drops_oldest_launch_deterministically() {
        let root = tempdir().expect("root");
        let executor = Arc::new(CompletingFixture::default());
        let manager = BackgroundProcessManager::new(
            Arc::new(IdentityCommandFixtureRedactor),
            BackgroundProcessLimits {
                max_completed_per_session: 1,
                ..BackgroundProcessLimits::default()
            },
        );
        let session = SessionId("retention".to_owned());
        let first = manager
            .start(executor.clone(), &session, request(root.path(), "first"))
            .expect("first");
        wait_for_status(&manager, &session, &first.process_id, false).await;
        let second = manager
            .start(executor, &session, request(root.path(), "second"))
            .expect("second");
        wait_for_status(&manager, &session, &second.process_id, false).await;
        let retained = manager.status(&session, None).expect("status");
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].process_id, second.process_id);
    }

    #[tokio::test]
    async fn record_and_replay_executors_fail_closed_before_background_launch() {
        let root = tempdir().expect("root");
        let fixtures = tempdir().expect("fixtures");
        let live = Arc::new(CompletingFixture::default());
        let recording_executor: Arc<dyn CommandExecutor> = Arc::new(
            RecordingCommandExecutor::new(live.clone(), fixtures.path(), root.path())
                .expect("recorder"),
        );
        let session = SessionId("record".to_owned());
        let manager = BackgroundProcessManager::new(
            Arc::new(IdentityCommandFixtureRedactor),
            BackgroundProcessLimits::default(),
        );
        assert!(
            manager
                .start(
                    recording_executor.clone(),
                    &session,
                    request(root.path(), "fixture"),
                )
                .is_err()
        );
        recording_executor
            .run(
                request(root.path(), "fixture"),
                CancellationToken::default(),
                Arc::new(crate::NoopOutputSink),
            )
            .await
            .expect("foreground recording remains available");
        assert_eq!(live.calls.load(Ordering::Relaxed), 1);

        let replay = ReplayCommandExecutor::load(fixtures.path(), root.path()).expect("replay");
        let replay_manager = BackgroundProcessManager::new(
            Arc::new(IdentityCommandFixtureRedactor),
            BackgroundProcessLimits::default(),
        );
        let replay_session = SessionId("replay".to_owned());
        assert!(
            replay_manager
                .start(
                    Arc::new(replay),
                    &replay_session,
                    request(root.path(), "fixture"),
                )
                .is_err()
        );
        assert_eq!(live.calls.load(Ordering::Relaxed), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_shutdown_kills_and_reaps_command_descendants() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("root");
        let policy = Arc::new(
            crate::SandboxPolicy::new([root.path()], crate::NetworkPolicy::Deny)
                .expect("sandbox policy"),
        );
        let manager = BackgroundProcessManager::new(
            Arc::new(IdentityCommandFixtureRedactor),
            BackgroundProcessLimits::default(),
        );
        let session = SessionId("descendants".to_owned());
        let started = manager
            .start(
                // The configured executor advertises real supervised-background
                // support. This fixture exercises process-group ownership only,
                // so the request deliberately bypasses sandbox-helper composition;
                // native sandbox enforcement has separate acceptance coverage.
                Arc::new(TokioCommandExecutor::default().sandboxed(policy)),
                &session,
                CommandRequest {
                    sandbox: BashSandboxMode::Unsandboxed,
                    ..request(root.path(), "sleep 60 & echo CHILD:$!; wait")
                },
            )
            .expect("start");
        let mut child_pid = None;
        for _ in 0..200 {
            let (snapshot, chunks) = manager
                .output(&session, &started.process_id, None)
                .expect("background process output");
            let rendered = chunks
                .iter()
                .map(|chunk| chunk.content.as_str())
                .collect::<String>();
            child_pid = rendered
                .split("CHILD:")
                .nth(1)
                .and_then(|value| value.lines().next())
                .and_then(|value| value.trim().parse::<i32>().ok());
            if child_pid.is_some() {
                break;
            }
            assert!(
                snapshot.status.running(),
                "background command completed before publishing its child pid: status={:?}, output={rendered:?}",
                snapshot.status
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = child_pid.expect("nonempty numeric child pid before deadline");
        let pid = rustix::process::Pid::from_raw(pid).expect("positive pid");
        assert!(rustix::process::test_kill_process(pid).is_ok());
        manager.shutdown_session(&session).await.expect("shutdown");
        for _ in 0..200 {
            if rustix::process::test_kill_process(pid).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("background descendant survived session shutdown")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delayed_background_writer_is_denied_by_read_only_sandbox() {
        let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
        let root = tempdir().expect("root");
        let policy = Arc::new(
            crate::SandboxPolicy::new([root.path()], crate::NetworkPolicy::Deny)
                .expect("sandbox policy"),
        );
        let executor: Arc<dyn CommandExecutor> =
            Arc::new(TokioCommandExecutor::default().sandboxed(policy));
        let manager = Arc::new(BackgroundProcessManager::new(
            Arc::new(IdentityCommandFixtureRedactor),
            BackgroundProcessLimits::default(),
        ));
        let tool =
            BashTool::new(executor, ToolLimits::default()).with_background_manager(manager.clone());
        let session = SessionId("delayed-writer".to_owned());
        let context = ToolContext::new(root.path())
            .expect("context")
            .with_session_id(session.clone());
        let result = tool
            .execute(
                &context,
                json!({
                    "command": "sleep 0.02; printf changed > delayed.txt",
                    "run_in_background": true
                }),
            )
            .await
            .expect("background launch");
        let process_id = result
            .data
            .pointer("/background_process/process_id")
            .and_then(Value::as_str)
            .expect("process id");
        for _ in 0..200 {
            let snapshot = manager
                .status(&session, Some(process_id))
                .expect("status")
                .remove(0);
            if !snapshot.status.running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !manager
                .status(&session, Some(process_id))
                .expect("final status")[0]
                .status
                .running()
        );
        assert!(!root.path().join("delayed.txt").exists());
    }
}
