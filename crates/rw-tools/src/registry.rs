use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use rw_types::{SessionId, SubagentId, SubagentResult, ToolCapability, ToolOutputStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Notify;

use crate::interaction::QuestionAsker;

/// Permission-relevant effects a tool may produce.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityManifest {
    capabilities: Vec<ToolCapability>,
}

impl CapabilityManifest {
    #[must_use]
    pub fn new(capabilities: impl IntoIterator<Item = ToolCapability>) -> Self {
        let mut unique = Vec::new();
        for capability in capabilities {
            if !unique.contains(&capability) {
                unique.push(capability);
            }
        }
        Self {
            capabilities: unique,
        }
    }

    #[must_use]
    pub fn capabilities(&self) -> &[ToolCapability] {
        &self.capabilities
    }

    #[must_use]
    pub fn contains(&self, capability: &ToolCapability) -> bool {
        self.capabilities.contains(capability)
    }
}

/// Discoverable metadata for a tool implementation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub capabilities: CapabilityManifest,
}

/// Global resource bounds for built-in tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolLimits {
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
    pub max_result_bytes: usize,
    pub max_search_results: usize,
    pub max_directory_entries: usize,
    pub max_web_bytes: usize,
}

/// Immutable authority for the generic MCP gateway tools.
///
/// Main sessions use [`Self::Unrestricted`] and are still constrained by the
/// host's approved MCP configuration. Child sessions use [`Self::Restricted`]
/// with exact canonical `mcp:<server>/<tool>` grants. Keeping this on the
/// invocation context prevents a generic `mcp_call` from widening a child
/// agent's declarative allowlist.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum McpToolPolicy {
    #[default]
    Unrestricted,
    Restricted(Arc<BTreeSet<String>>),
}

impl McpToolPolicy {
    /// Builds a fail-closed policy from canonical virtual MCP tool ids.
    ///
    /// # Errors
    ///
    /// Rejects malformed, non-canonical, duplicate, or oversized entries.
    pub fn restricted(entries: impl IntoIterator<Item = String>) -> Result<Self, ToolError> {
        let mut grants = BTreeSet::new();
        for entry in entries {
            validate_mcp_virtual_tool(&entry)?;
            if !grants.insert(entry.clone()) {
                return Err(ToolError::InvalidInput(format!(
                    "duplicate MCP virtual tool grant: {entry}"
                )));
            }
            if grants.len() > 128 {
                return Err(ToolError::InvalidInput(
                    "MCP virtual tool allowlist exceeds 128 entries".to_owned(),
                ));
            }
        }
        Ok(Self::Restricted(Arc::new(grants)))
    }

    #[must_use]
    pub fn allows(&self, server: &str, tool: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Restricted(grants) => grants.contains(&format!("mcp:{server}/{tool}")),
        }
    }

    #[must_use]
    pub fn grants(&self) -> Option<&BTreeSet<String>> {
        match self {
            Self::Unrestricted => None,
            Self::Restricted(grants) => Some(grants),
        }
    }
}

/// Validates the exact declarative spelling used for an MCP tool grant.
/// Wildcards are intentionally unsupported.
///
/// # Errors
///
/// Returns when the entry is not canonical `mcp:<server>/<tool>` syntax.
pub fn validate_mcp_virtual_tool(entry: &str) -> Result<(), ToolError> {
    let Some(rest) = entry.strip_prefix("mcp:") else {
        return Err(ToolError::InvalidInput(format!(
            "MCP virtual tool must use mcp:<server>/<tool>: {entry}"
        )));
    };
    let Some((server, tool)) = rest.split_once('/') else {
        return Err(ToolError::InvalidInput(format!(
            "MCP virtual tool must use mcp:<server>/<tool>: {entry}"
        )));
    };
    let valid_server = !server.is_empty()
        && server.len() <= 96
        && server
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    let valid_tool = !tool.is_empty()
        && tool.len() <= 256
        && !tool
            .chars()
            .any(|character| character.is_control() || character.is_whitespace());
    if !valid_server || !valid_tool || tool.contains('*') {
        return Err(ToolError::InvalidInput(format!(
            "invalid MCP virtual tool grant: {entry}"
        )));
    }
    Ok(())
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: 1024 * 1024,
            max_write_bytes: 4 * 1024 * 1024,
            max_result_bytes: 256 * 1024,
            max_search_results: 2_000,
            max_directory_entries: 10_000,
            max_web_bytes: 512 * 1024,
        }
    }
}

/// Cooperative cancellation shared by the engine and a running tool.
#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

#[derive(Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn check(&self) -> Result<(), ToolError> {
        if self.is_cancelled() {
            Err(ToolError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// One live output fragment, normally emitted by an executing shell command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolOutputChunk {
    pub stream: ToolOutputStream,
    pub content: String,
}

/// Engine/TUI boundary for live tool output.
#[async_trait]
pub trait ToolOutputSink: Send + Sync {
    async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError>;
}

/// Lifecycle records emitted by tools that create full child agent sessions.
///
/// Core supplies this sink after the ordinary capability/permission gate. The
/// sink is deliberately unavailable to tools executed outside a session actor,
/// preventing extensions from forging durable parent-session records.
#[derive(Clone, Debug, PartialEq)]
pub enum SubagentLifecycleEvent {
    Spawned {
        subagent_id: SubagentId,
        child_session_id: SessionId,
        task: String,
    },
    Finished {
        subagent_id: SubagentId,
        result: Box<SubagentResult>,
    },
}

/// One child event forwarded only to the active parent client for display.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentProgressEvent {
    pub subagent_id: SubagentId,
    pub child_session_id: SessionId,
    pub child_sequence: Option<u64>,
    pub event: Value,
}

/// Engine-owned bridge for durable lifecycle and display-only child progress.
#[async_trait]
pub trait SubagentEventSink: Send + Sync {
    async fn lifecycle(&self, event: SubagentLifecycleEvent) -> Result<(), ToolError>;

    async fn progress(&self, event: SubagentProgressEvent) -> Result<(), ToolError>;
}

#[derive(Debug, Default)]
pub struct NoopOutputSink;

#[async_trait]
impl ToolOutputSink for NoopOutputSink {
    async fn emit(&self, _chunk: ToolOutputChunk) -> Result<(), ToolError> {
        Ok(())
    }
}

/// Replaceable operational status. It is never tool output or a durable record.
pub trait ToolProgressSink: Send + Sync {
    /// # Errors
    /// Rejects progress after the invocation's admission has closed.
    fn report(&self, progress: rw_operation_contract::ToolProgress) -> Result<(), ToolError>;
}

#[derive(Debug, Default)]
pub struct NoopProgressSink;
impl ToolProgressSink for NoopProgressSink {
    fn report(&self, _progress: rw_operation_contract::ToolProgress) -> Result<(), ToolError> {
        Ok(())
    }
}

/// Per-invocation context supplied by core after permission approval.
#[derive(Clone)]
pub struct ToolContext {
    workspace_roots: Arc<Vec<PathBuf>>,
    #[cfg(unix)]
    workspace_fds: Arc<Vec<OwnedFd>>,
    session_id: Option<SessionId>,
    model_alias: Option<String>,
    native_searcher: Option<Arc<dyn crate::WebSearcher>>,
    effect_domains: Option<Arc<[String]>>,
    effect_paths: Option<Arc<[PathBuf]>>,
    effect_host: Option<Arc<dyn crate::ToolEffectHost>>,
    todo_store: Option<Arc<dyn crate::TodoStateStore>>,
    result_limit_bytes: usize,
    pub cancellation: CancellationToken,
    pub output: Arc<dyn ToolOutputSink>,
    pub progress: Arc<dyn ToolProgressSink>,
    question_asker: Option<Arc<dyn QuestionAsker>>,
    subagent_events: Option<Arc<dyn SubagentEventSink>>,
    mcp_tool_policy: McpToolPolicy,
}

impl ToolContext {
    /// Create an invocation context rooted at an existing workspace directory.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Io`] when the workspace root cannot be canonicalized.
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, ToolError> {
        Self::from_workspace_roots([workspace_root.as_ref()])
    }

    /// Create an invocation context with one primary and zero or more added
    /// workspace roots. Relative paths resolve against the primary root;
    /// additional roots use the stable `@root/<index>/...` virtual prefix.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Io`] when any root cannot be canonicalized and
    /// pinned as a directory.
    pub fn from_workspace_roots<Roots, Root>(workspace_roots: Roots) -> Result<Self, ToolError>
    where
        Roots: IntoIterator<Item = Root>,
        Root: AsRef<Path>,
    {
        let mut canonical_roots = Vec::new();
        for root in workspace_roots {
            let supplied = root.as_ref();
            let canonical = std::fs::canonicalize(supplied).map_err(|source| ToolError::Io {
                operation: "canonicalize workspace",
                path: supplied.to_path_buf(),
                source,
            })?;
            if !canonical.is_dir() {
                return Err(ToolError::InvalidInput(format!(
                    "workspace root is not a directory: {}",
                    supplied.display()
                )));
            }
            if !canonical_roots.contains(&canonical) {
                canonical_roots.push(canonical);
            }
        }
        if canonical_roots.is_empty() {
            return Err(ToolError::InvalidInput(
                "at least one workspace root is required".to_owned(),
            ));
        }
        #[cfg(unix)]
        let workspace_fds = canonical_roots
            .iter()
            .map(|canonical| {
                rustix::fs::open(
                    canonical,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )
                .map_err(|source| ToolError::Io {
                    operation: "open workspace directory",
                    path: canonical.clone(),
                    source: source.into(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            workspace_roots: Arc::new(canonical_roots),
            #[cfg(unix)]
            workspace_fds: Arc::new(workspace_fds),
            session_id: None,
            model_alias: None,
            native_searcher: None,
            effect_domains: None,
            effect_paths: None,
            effect_host: None,
            todo_store: None,
            result_limit_bytes: ToolLimits::default().max_result_bytes,
            cancellation: CancellationToken::default(),
            output: Arc::new(NoopOutputSink),
            progress: Arc::new(NoopProgressSink),
            question_asker: None,
            subagent_events: None,
            mcp_tool_policy: McpToolPolicy::Unrestricted,
        })
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_roots[0]
    }

    /// Canonical write/read roots in primary-first order.
    #[must_use]
    pub fn workspace_roots(&self) -> &[PathBuf] {
        &self.workspace_roots
    }

    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    #[must_use]
    pub fn with_session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Bind the model alias selected for this exact turn. Tools must not cache
    /// this across turns because `/model` and custom-command overrides can
    /// select a different provider backend.
    #[must_use]
    pub fn with_model_alias(mut self, model_alias: impl Into<String>) -> Self {
        self.model_alias = Some(model_alias.into());
        self
    }

    #[must_use]
    pub fn model_alias(&self) -> Option<&str> {
        self.model_alias.as_deref()
    }

    #[must_use]
    pub fn with_todo_store(mut self, store: Arc<dyn crate::TodoStateStore>) -> Self {
        self.todo_store = Some(store);
        self
    }

    #[must_use]
    pub fn todo_store(&self) -> Option<&Arc<dyn crate::TodoStateStore>> {
        self.todo_store.as_ref()
    }

    /// Bind the exact approved outer invocation's host effect lane.
    #[must_use]
    pub fn with_effect_host(mut self, host: Arc<dyn crate::ToolEffectHost>) -> Self {
        self.effect_host = Some(host);
        self
    }

    #[must_use]
    pub fn effect_host(&self) -> Option<Arc<dyn crate::ToolEffectHost>> {
        self.effect_host.clone()
    }

    pub(crate) fn without_effect_host(mut self) -> Self {
        self.effect_host = None;
        self
    }

    pub(crate) fn with_effect_paths(mut self, paths: Arc<[PathBuf]>) -> Self {
        self.effect_paths = Some(paths);
        self
    }

    pub(crate) fn with_effect_domains(mut self, domains: Arc<[String]>) -> Self {
        self.effect_domains = Some(domains);
        self
    }

    pub(crate) fn effect_domains(&self) -> Option<Arc<[String]>> {
        self.effect_domains.clone()
    }

    /// Bind the admitted native backend for this turn; callbacks never enter tool JSON.
    #[must_use]
    pub fn with_native_searcher(mut self, searcher: Option<Arc<dyn crate::WebSearcher>>) -> Self {
        self.native_searcher = searcher;
        self
    }

    #[must_use]
    pub fn native_searcher(&self) -> Option<&Arc<dyn crate::WebSearcher>> {
        self.native_searcher.as_ref()
    }

    #[must_use]
    pub fn with_mcp_tool_policy(mut self, policy: McpToolPolicy) -> Self {
        self.mcp_tool_policy = policy;
        self
    }

    #[must_use]
    pub fn mcp_tool_policy(&self) -> &McpToolPolicy {
        &self.mcp_tool_policy
    }

    #[must_use]
    pub fn result_limit_bytes(&self) -> usize {
        self.result_limit_bytes
    }

    #[must_use]
    pub fn with_result_limit(mut self, limit: usize) -> Self {
        self.result_limit_bytes = limit;
        self
    }

    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    #[must_use]
    pub fn with_output(mut self, output: Arc<dyn ToolOutputSink>) -> Self {
        self.output = output;
        self
    }

    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn ToolProgressSink>) -> Self {
        self.progress = progress;
        self
    }

    /// Routes interactive questions through the engine protocol for this
    /// invocation rather than through a frontend-specific side channel.
    #[must_use]
    pub fn with_question_asker(mut self, asker: Arc<dyn QuestionAsker>) -> Self {
        self.question_asker = Some(asker);
        self
    }

    /// Installs the engine-owned child-session event bridge for this exact
    /// invocation. This is used by the public `spawn_agent` tool API.
    #[must_use]
    pub fn with_subagent_event_sink(mut self, sink: Arc<dyn SubagentEventSink>) -> Self {
        self.subagent_events = Some(sink);
        self
    }

    /// Returns the child-session event bridge when execution is actor-owned.
    #[must_use]
    pub fn subagent_event_sink(&self) -> Option<&Arc<dyn SubagentEventSink>> {
        self.subagent_events.as_ref()
    }

    pub(crate) fn question_asker(&self) -> Option<&Arc<dyn QuestionAsker>> {
        self.question_asker.as_ref()
    }

    pub(crate) fn resolve_existing(&self, path: &Path) -> Result<PathBuf, ToolError> {
        let candidate = self.candidate_path(path)?;
        let canonical = std::fs::canonicalize(&candidate).map_err(|source| ToolError::Io {
            operation: "resolve path",
            path: path.to_path_buf(),
            source,
        })?;
        if self.root_index_for(&canonical).is_none() {
            return Err(ToolError::PathEscape(path.to_path_buf()));
        }
        self.check_effect_path(&canonical)?;
        Ok(canonical)
    }

    fn check_effect_path(&self, path: &Path) -> Result<(), ToolError> {
        if self
            .effect_paths
            .as_ref()
            .is_some_and(|paths| !paths.iter().any(|allowed| allowed == path))
        {
            return Err(ToolError::DelegationDenied(
                "file IO exceeds the captured checkpoint paths".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn resolve_writable(&self, path: &Path) -> Result<PathBuf, ToolError> {
        if path.as_os_str().is_empty() {
            return Err(ToolError::InvalidInput("path must not be empty".to_owned()));
        }
        if path
            .components()
            .any(|component| matches!(component, Component::Prefix(_)))
        {
            return Err(ToolError::PathEscape(path.to_path_buf()));
        }
        let candidate = self.candidate_path(path)?;
        if candidate.exists() {
            return self.resolve_existing(&candidate);
        }
        let parent = candidate
            .parent()
            .ok_or_else(|| ToolError::PathEscape(path.into()))?;
        let canonical_parent = std::fs::canonicalize(parent).map_err(|source| ToolError::Io {
            operation: "resolve parent directory",
            path: parent.to_path_buf(),
            source,
        })?;
        if self.root_index_for(&canonical_parent).is_none() {
            return Err(ToolError::PathEscape(path.to_path_buf()));
        }
        let file_name = candidate
            .file_name()
            .ok_or_else(|| ToolError::InvalidInput("path must name a file".to_owned()))?;
        let resolved = canonical_parent.join(file_name);
        self.check_effect_path(&resolved)?;
        Ok(resolved)
    }

    pub(crate) fn relative_display(&self, path: &Path) -> PathBuf {
        let Some(index) = self.root_index_for(path) else {
            return path.to_path_buf();
        };
        let relative = path
            .strip_prefix(&self.workspace_roots[index])
            .unwrap_or(path);
        if index == 0 {
            relative.to_path_buf()
        } else {
            PathBuf::from("@root")
                .join(index.to_string())
                .join(relative)
        }
    }

    pub(crate) fn resolve_search_roots(&self, path: &Path) -> Result<Vec<PathBuf>, ToolError> {
        if path == Path::new(".") && self.workspace_roots.len() > 1 {
            Ok(self.workspace_roots.as_ref().clone())
        } else {
            self.resolve_existing(path).map(|path| vec![path])
        }
    }

    fn candidate_path(&self, path: &Path) -> Result<PathBuf, ToolError> {
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        let mut components = path.components();
        if components.next().is_some_and(
            |component| matches!(component, Component::Normal(name) if name == "@root"),
        ) {
            let Some(Component::Normal(index)) = components.next() else {
                return Err(ToolError::InvalidInput(
                    "virtual root path must use @root/<index>/...".to_owned(),
                ));
            };
            let index = index
                .to_str()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|index| *index > 0)
                .ok_or_else(|| {
                    ToolError::InvalidInput(
                        "virtual root index must be a positive integer".to_owned(),
                    )
                })?;
            let root = self
                .workspace_roots
                .get(index)
                .ok_or_else(|| ToolError::PathEscape(path.to_path_buf()))?;
            return Ok(components.fold(root.clone(), |joined, component| {
                joined.join(component.as_os_str())
            }));
        }
        Ok(self.workspace_root().join(path))
    }

    fn root_index_for(&self, path: &Path) -> Option<usize> {
        self.workspace_roots
            .iter()
            .enumerate()
            .filter(|(_, root)| path.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
            .map(|(index, _)| index)
    }

    /// Traverse a canonical workspace-relative parent using pinned, no-follow directory handles.
    /// This closes the symlink-swap window for direct read/write/edit operations on Unix.
    #[cfg(unix)]
    pub(crate) fn secure_parent(&self, path: &Path) -> Result<(OwnedFd, OsString), ToolError> {
        self.check_effect_path(path)?;
        let root_index = self
            .root_index_for(path)
            .ok_or_else(|| ToolError::PathEscape(path.to_path_buf()))?;
        let root = &self.workspace_roots[root_index];
        let relative = path
            .strip_prefix(root)
            .map_err(|_| ToolError::PathEscape(path.to_path_buf()))?;
        let file_name = relative
            .file_name()
            .ok_or_else(|| ToolError::InvalidInput("path must name a file".to_owned()))?
            .to_os_string();
        let mut directory = self
            .workspace_fds
            .get(root_index)
            .ok_or_else(|| ToolError::PathEscape(path.to_path_buf()))?
            .try_clone()
            .map_err(|source| ToolError::Io {
                operation: "clone workspace directory handle",
                path: root.clone(),
                source,
            })?;
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                let Component::Normal(name) = component else {
                    continue;
                };
                directory = rustix::fs::openat(
                    &directory,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )
                .map_err(|source| ToolError::Io {
                    operation: "open workspace subdirectory without following links",
                    path: path.to_path_buf(),
                    source: source.into(),
                })?;
            }
        }
        Ok((directory, file_name))
    }
}

/// Structured result passed back to the model and UI.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema, ts_rs::TS)]
#[serde(deny_unknown_fields)]
#[ts(rename = "ToolResponse")]
pub struct ToolResult {
    #[serde(skip)]
    presentation: Option<crate::ToolPresentationPlan>,
    pub content: String,
    pub data: Value,
    pub truncated: bool,
    #[serde(skip)]
    protected_framing: Option<ProtectedFraming>,
}

#[derive(Clone, Debug, PartialEq)]
struct ProtectedFraming {
    prefix: String,
    suffix: String,
}

impl ToolResult {
    #[must_use]
    pub fn with_presentation(mut self, presentation: crate::ToolPresentationPlan) -> Self {
        self.presentation = Some(presentation);
        self
    }
    pub fn take_presentation(&mut self) -> Option<crate::ToolPresentationPlan> {
        self.presentation.take()
    }

    #[must_use]
    pub fn new(content: impl Into<String>, data: Value) -> Self {
        Self {
            content: content.into(),
            data,
            truncated: false,
            protected_framing: None,
            presentation: None,
        }
    }

    /// Protect framing that must survive central result truncation, such as untrusted-data guards.
    #[must_use]
    pub fn with_protected_framing(
        mut self,
        prefix: impl Into<String>,
        suffix: impl Into<String>,
    ) -> Self {
        self.protected_framing = Some(ProtectedFraming {
            prefix: prefix.into(),
            suffix: suffix.into(),
        });
        self
    }

    fn enforce_wire_limit(mut self, limit: usize) -> Result<Self, ToolError> {
        let Ok(encoded) = serde_json::to_vec(&self) else {
            return Ok(self);
        };
        if encoded.len() <= limit {
            return Ok(self);
        }
        let original_wire_bytes = encoded.len();
        self.truncated = true;
        self.data = serde_json::json!({
            "truncated": true,
            "original_wire_bytes": original_wire_bytes,
        });
        let original_content = std::mem::take(&mut self.content);
        if let Some(framing) = self.protected_framing.clone() {
            let body = original_content
                .strip_prefix(&framing.prefix)
                .and_then(|value| value.strip_suffix(&framing.suffix))
                .ok_or_else(|| {
                    ToolError::Output("protected result framing did not match content".to_owned())
                })?;
            self.content = format!("{}{}", framing.prefix, framing.suffix);
            if serde_json::to_vec(&self).is_ok_and(|value| value.len() > limit) {
                return Err(ToolError::SizeLimit { limit });
            }
            let boundaries: Vec<usize> = body
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(body.len()))
                .collect();
            let mut low = 0usize;
            let mut high = boundaries.len().saturating_sub(1);
            while low < high {
                let retained_characters = low + (high - low).div_ceil(2);
                let start = boundaries[boundaries.len() - retained_characters - 1];
                self.content = format!("{}{}{}", framing.prefix, &body[start..], framing.suffix);
                if serde_json::to_vec(&self).is_ok_and(|value| value.len() <= limit) {
                    low = retained_characters;
                } else {
                    high = retained_characters.saturating_sub(1);
                }
            }
            let start = boundaries[boundaries.len() - low - 1];
            self.content = format!("{}{}{}", framing.prefix, &body[start..], framing.suffix);
            return Ok(self);
        }
        let boundaries: Vec<usize> = original_content
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(original_content.len()))
            .collect();
        let mut low = 0usize;
        let mut high = boundaries.len().saturating_sub(1);
        while low < high {
            let retained_characters = low + (high - low).div_ceil(2);
            let start = boundaries[boundaries.len() - retained_characters - 1];
            original_content[start..].clone_into(&mut self.content);
            let fits = serde_json::to_vec(&self).is_ok_and(|value| value.len() <= limit);
            if fits {
                low = retained_characters;
            } else {
                high = retained_characters.saturating_sub(1);
            }
        }
        let start = boundaries[boundaries.len() - low - 1];
        original_content[start..].clone_into(&mut self.content);
        if serde_json::to_vec(&self).is_ok_and(|value| value.len() > limit) {
            self.content.clear();
            self.data = Value::Null;
        }
        if serde_json::to_vec(&self).is_ok_and(|value| value.len() > limit) {
            Err(ToolError::SizeLimit { limit })
        } else {
            Ok(self)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CandidateLocation {
    pub line: usize,
    pub column: usize,
}

/// Stable errors suitable for model-visible tool failures.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool effect settlement is unproven: {0}")]
    EffectsUnsettled(String),
    #[error("delegated effect denied: {0}")]
    DelegationDenied(String),
    #[error("invalid tool input: {0}")]
    InvalidInput(String),
    #[error("path escapes the workspace: {0}")]
    PathEscape(PathBuf),
    #[error("content exceeds the {limit}-byte limit")]
    SizeLimit { limit: usize },
    #[error("edit target was not found")]
    EditNotFound,
    #[error("edit target is ambiguous at {candidates:?}")]
    AmbiguousEdit { candidates: Vec<CandidateLocation> },
    #[error("file changed since read: {0}")]
    FileChangedSinceRead(PathBuf),
    #[error("isolated worktree changed after capture: {0}")]
    WorktreeChangedAfterCapture(PathBuf),
    #[error("isolated worktree has a running background process: {0}")]
    WorktreeProcessRunning(PathBuf),
    #[error("isolated worktree is being finalized: {0}")]
    WorktreeFinalizing(PathBuf),
    #[error("tool invocation was cancelled")]
    Cancelled,
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("command failed: {0}")]
    Command(String),
    #[error("network fetch failed: {0}")]
    Network(String),
    #[error("interaction failed: {0}")]
    Interaction(String),
    #[error("symbol index failed: {0}")]
    Intelligence(String),
    #[error("output stream failed: {0}")]
    Output(String),
    #[error("tool is already registered: {0}")]
    DuplicateTool(String),
}

/// Filesystem mutation extent used by core to checkpoint before execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "scope", content = "paths")]
pub enum MutationScope {
    None,
    Paths(Vec<PathBuf>),
    OpaqueWorkspace,
}

/// Stable behavior categories consumed by engine and host policy adapters.
///
/// A tool implementation owns this classification. Callers must resolve it
/// through [`ToolRegistry`] instead of inferring behavior from the tool name.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolBehavior {
    #[default]
    Standard,
    FileMutation,
    Shell,
    WebFetch,
    UserInteraction,
    PlanSubmission,
    BackgroundControl,
}

/// Tool-owned semantic projection of one registered invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolInvocationSemantics {
    pub behavior: ToolBehavior,
    pub mutation_scope: MutationScope,
    pub workspace_paths: Vec<PathBuf>,
}

/// Whether a tool implementation captures workspace-root-specific state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceBinding {
    RootBound,
    RootIndependent,
}

/// Durable child-lifecycle production declared by any public tool extension.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SubagentLifecycleMode {
    #[default]
    None,
    Single,
    MultipleOrdered,
}

/// Exact filesystem mutation preview used to bind a visual approval.
///
/// Tools opt in only when they can derive the bytes they will write from the
/// same validated input and workspace state used by execution. Core hashes
/// this value and recomputes it immediately before invoking the tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalPreview {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
}

/// Public extension point implemented identically by first- and third-party tools.
#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    /// Declares behavior that policy adapters cannot safely infer from a name.
    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::Standard
    }

    /// Whether this implementation uses a host-owned nested effect lane.
    fn delegates_effects(&self) -> bool {
        false
    }

    /// Source-owned support for calls made under another tool's approval.
    /// Unspecified tools cannot acquire nested effects from capability flags.
    ///
    /// # Errors
    /// Returns malformed typed input before delegated execution.
    fn delegated_effect(&self, _input: &Value) -> Result<crate::DelegatedEffect, ToolError> {
        Ok(crate::DelegatedEffect::Denied)
    }

    /// Returns workspace paths explicitly carried by this invocation.
    ///
    /// Implementations should parse their typed input here exactly as they do
    /// at execution. The empty default means the tool has no declared path,
    /// not that an unknown input field named `path` is trusted.
    ///
    /// # Errors
    ///
    /// Returns an error when the invocation input cannot be parsed safely.
    fn workspace_paths(&self, _input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(Vec::new())
    }

    /// Root-bound is the fail-closed default. Only pure orchestration controls
    /// that never resolve workspace paths may opt into root independence.
    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootBound
    }

    /// Declares whether this tool emits durable child lifecycle events.
    fn subagent_lifecycle_mode(&self) -> SubagentLifecycleMode {
        SubagentLifecycleMode::None
    }

    /// Input-dependent capabilities inspected by core before approval.
    /// The guarded registry always unions these with the descriptor snapshot,
    /// so an implementation cannot hide a statically declared capability.
    ///
    /// # Errors
    ///
    /// Returns an error when the invocation input cannot be safely classified.
    fn invocation_capabilities(&self, _input: &Value) -> Result<CapabilityManifest, ToolError> {
        Ok(self.descriptor().capabilities)
    }

    /// Whether this exact invocation may overlap another parallel-safe call.
    /// Core still performs capability approval independently.
    fn parallel_safe(&self, input: &Value) -> bool {
        let descriptor = self.descriptor();
        matches!(self.mutation_scope(input), MutationScope::None)
            && !descriptor.capabilities.capabilities().is_empty()
            && descriptor
                .capabilities
                .capabilities()
                .iter()
                .all(|capability| matches!(capability, ToolCapability::ReadFilesystem))
    }

    /// Describe possible workspace mutation for this input before execution.
    ///
    /// The default fails safe for tools declaring filesystem writes. Tools that can resolve a
    /// narrower path set should override this method.
    fn mutation_scope(&self, input: &Value) -> MutationScope {
        if self
            .descriptor()
            .capabilities
            .contains(&ToolCapability::WriteFilesystem)
        {
            match self.workspace_paths(input) {
                Ok(paths)
                    if !paths.is_empty()
                        && paths.iter().all(|path| {
                            !path.as_os_str().is_empty()
                                && !path.is_absolute()
                                && path.components().all(|component| {
                                    matches!(component, std::path::Component::Normal(_))
                                })
                        }) =>
                {
                    MutationScope::Paths(paths)
                }
                Ok(_) | Err(_) => MutationScope::OpaqueWorkspace,
            }
        } else {
            MutationScope::None
        }
    }

    /// Return an exact, workspace-derived preview for a visually approved
    /// mutation. The default preserves generic approvals for tools without a
    /// meaningful unified diff.
    async fn approval_preview(
        &self,
        _context: &ToolContext,
        _input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        Ok(None)
    }

    /// Releases resources owned by an ending actor session. Implementations
    /// must be idempotent because a host may retry cleanup after an error.
    async fn end_session(&self, _session_id: &SessionId) -> Result<(), ToolError> {
        Ok(())
    }

    /// Human-readable active resource which makes idle-sensitive engine
    /// operations fail closed.
    fn session_activity(&self, _session_id: &SessionId) -> Option<String> {
        None
    }

    /// Marks a tool whose lifecycle observer must survive filtered registries.
    fn observes_session_resources(&self) -> bool {
        false
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError>;

    /// Waits for externally owned effects after an execution future is dropped.
    /// A failed proof is an error; it cannot authorize releasing owned mutation resources.
    async fn settle_effects(&self) -> Result<(), ToolError>;
}

#[derive(Clone)]
struct RegisteredTool {
    tool: Arc<dyn Tool>,
    descriptor: ToolDescriptor,
    behavior: ToolBehavior,
    subagent_lifecycle_mode: SubagentLifecycleMode,
}

struct GuardedTool {
    inner: Arc<dyn Tool>,
    descriptor: ToolDescriptor,
    behavior: ToolBehavior,
    subagent_lifecycle_mode: SubagentLifecycleMode,
}

#[async_trait]
impl Tool for GuardedTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        self.inner.settle_effects().await
    }
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        self.inner.workspace_binding()
    }

    fn behavior(&self) -> ToolBehavior {
        self.behavior
    }

    fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        self.inner.workspace_paths(input)
    }

    fn delegated_effect(&self, input: &Value) -> Result<crate::DelegatedEffect, ToolError> {
        self.inner.delegated_effect(input)
    }

    fn delegates_effects(&self) -> bool {
        self.inner.delegates_effects()
    }

    fn subagent_lifecycle_mode(&self) -> SubagentLifecycleMode {
        self.subagent_lifecycle_mode
    }

    fn mutation_scope(&self, input: &Value) -> MutationScope {
        let scope = self.inner.mutation_scope(input);
        if self
            .descriptor
            .capabilities
            .contains(&ToolCapability::WriteFilesystem)
            && matches!(scope, MutationScope::None)
        {
            MutationScope::OpaqueWorkspace
        } else {
            scope
        }
    }

    fn invocation_capabilities(&self, input: &Value) -> Result<CapabilityManifest, ToolError> {
        let dynamic = self.inner.invocation_capabilities(input)?;
        Ok(CapabilityManifest::new(
            self.descriptor
                .capabilities
                .capabilities()
                .iter()
                .cloned()
                .chain(dynamic.capabilities().iter().cloned()),
        ))
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        self.inner.parallel_safe(input) && matches!(self.mutation_scope(input), MutationScope::None)
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        self.inner.approval_preview(context, input).await
    }

    async fn end_session(&self, session_id: &SessionId) -> Result<(), ToolError> {
        self.inner.end_session(session_id).await
    }

    fn session_activity(&self, session_id: &SessionId) -> Option<String> {
        self.inner.session_activity(session_id)
    }

    fn observes_session_resources(&self) -> bool {
        self.inner.observes_session_resources()
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        self.inner
            .execute(context, input)
            .await
            .and_then(|result| result.enforce_wire_limit(context.result_limit_bytes()))
    }
}

/// Deterministic registry. It resolves tools; core remains responsible for permission checks.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
    mcp_tool_policy: McpToolPolicy,
    session_observers: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool without replacing an existing name.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::DuplicateTool`] when the name is already registered.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let descriptor = tool.descriptor();
        let behavior = tool.behavior();
        let subagent_lifecycle_mode = tool.subagent_lifecycle_mode();
        let name = descriptor.name.clone();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(ToolError::InvalidInput(format!(
                "tool name must use lowercase ASCII letters, digits, or underscores: {name}"
            )));
        }
        if self.tools.contains_key(&name) {
            return Err(ToolError::DuplicateTool(name));
        }
        let guarded: Arc<dyn Tool> = Arc::new(GuardedTool {
            inner: tool,
            descriptor: descriptor.clone(),
            behavior,
            subagent_lifecycle_mode,
        });
        if guarded.observes_session_resources() {
            self.session_observers.push(Arc::clone(&guarded));
        }
        self.tools.insert(
            name,
            RegisteredTool {
                tool: guarded,
                descriptor,
                behavior,
                subagent_lifecycle_mode,
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .get(name)
            .map(|registered| Arc::clone(&registered.tool))
    }

    #[must_use]
    pub fn descriptor(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools
            .get(name)
            .map(|registered| &registered.descriptor)
    }

    #[must_use]
    pub fn subagent_lifecycle_mode(&self, name: &str) -> Option<SubagentLifecycleMode> {
        self.tools
            .get(name)
            .map(|registered| registered.subagent_lifecycle_mode)
    }

    /// Resolve a mutation hint while enforcing the registered capability snapshot.
    #[must_use]
    pub fn mutation_scope(&self, name: &str, input: &Value) -> Option<MutationScope> {
        self.tools.get(name).map(|registered| {
            let scope = registered.tool.mutation_scope(input);
            if registered
                .descriptor
                .capabilities
                .contains(&ToolCapability::WriteFilesystem)
                && matches!(scope, MutationScope::None)
            {
                MutationScope::OpaqueWorkspace
            } else {
                scope
            }
        })
    }

    /// Resolves tool-owned behavior, mutation, and path semantics together.
    ///
    /// Unknown tools return `Ok(None)` so callers can fail closed without
    /// inventing defaults. Malformed registered input returns the tool's parse
    /// error and must likewise be rejected before policy or hook dispatch.
    ///
    /// # Errors
    ///
    /// Returns the registered tool's input-classification error.
    pub fn invocation_semantics(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<Option<ToolInvocationSemantics>, ToolError> {
        let Some(registered) = self.tools.get(name) else {
            return Ok(None);
        };
        Ok(Some(ToolInvocationSemantics {
            behavior: registered.behavior,
            mutation_scope: registered.tool.mutation_scope(input),
            workspace_paths: registered.tool.workspace_paths(input)?,
        }))
    }

    /// Registered names in one tool-owned behavior category.
    #[must_use]
    pub fn names_with_behavior(&self, behavior: ToolBehavior) -> Vec<&str> {
        self.tools
            .iter()
            .filter_map(|(name, registered)| {
                (registered.behavior == behavior).then_some(name.as_str())
            })
            .collect()
    }

    /// Inspect immutable declarations without first cloning their JSON schemas.
    pub fn descriptor_refs(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.tools.values().map(|registered| &registered.descriptor)
    }

    #[must_use]
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    /// Runs the idempotent session cleanup hook for every registered tool.
    ///
    /// # Errors
    ///
    /// Returns the first cleanup error after still attempting every tool.
    pub async fn end_session(&self, session_id: &SessionId) -> Result<(), ToolError> {
        let mut first_error = None;
        for registered in self.tools.values() {
            if registered.tool.observes_session_resources() {
                continue;
            }
            if let Err(error) = registered.tool.end_session(session_id).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        for observer in &self.session_observers {
            if let Err(error) = observer.end_session(session_id).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    #[must_use]
    pub fn session_activity(&self, session_id: &SessionId) -> Option<String> {
        self.session_observers
            .iter()
            .find_map(|observer| observer.session_activity(session_id))
    }

    /// Builds a registry containing only the exact requested tool names.
    /// Existing guarded registrations are shared; no implementation is
    /// reconstructed and capability snapshots remain unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidInput`] if any requested name is unknown.
    pub fn subset<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Result<Self, ToolError> {
        let mut tools = BTreeMap::new();
        for name in names {
            let registered = self.tools.get(name).ok_or_else(|| {
                ToolError::InvalidInput(format!("allowed tool is not registered: {name}"))
            })?;
            tools
                .entry(name.to_owned())
                .or_insert_with(|| registered.clone());
        }
        Ok(Self {
            tools,
            mcp_tool_policy: self.mcp_tool_policy.clone(),
            session_observers: self.session_observers.clone(),
        })
    }

    /// Binds the immutable MCP gateway policy copied into each invocation.
    #[must_use]
    pub fn with_mcp_tool_policy(mut self, policy: McpToolPolicy) -> Self {
        self.mcp_tool_policy = policy;
        self
    }

    #[must_use]
    pub fn mcp_tool_policy(&self) -> &McpToolPolicy {
        &self.mcp_tool_policy
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

pub(crate) fn input_schema<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

pub(crate) fn parse_input<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|error| ToolError::InvalidInput(error.to_string()))
}

#[cfg(test)]
mod tests;
