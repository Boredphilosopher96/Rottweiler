use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use rw_types::{SessionId, ToolCapability, ToolOutputStream};
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

#[derive(Debug, Default)]
pub struct NoopOutputSink;

#[async_trait]
impl ToolOutputSink for NoopOutputSink {
    async fn emit(&self, _chunk: ToolOutputChunk) -> Result<(), ToolError> {
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
    result_limit_bytes: usize,
    pub cancellation: CancellationToken,
    pub output: Arc<dyn ToolOutputSink>,
    question_asker: Option<Arc<dyn QuestionAsker>>,
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
            result_limit_bytes: ToolLimits::default().max_result_bytes,
            cancellation: CancellationToken::default(),
            output: Arc::new(NoopOutputSink),
            question_asker: None,
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

    /// Routes interactive questions through the engine protocol for this
    /// invocation rather than through a frontend-specific side channel.
    #[must_use]
    pub fn with_question_asker(mut self, asker: Arc<dyn QuestionAsker>) -> Self {
        self.question_asker = Some(asker);
        self
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
        Ok(canonical)
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
        Ok(canonical_parent.join(file_name))
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
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolResult {
    pub content: String,
    pub data: Value,
    #[serde(default)]
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
    pub fn new(content: impl Into<String>, data: Value) -> Self {
        Self {
            content: content.into(),
            data,
            truncated: false,
            protected_framing: None,
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

    /// Describe possible workspace mutation for this input before execution.
    ///
    /// The default fails safe for tools declaring filesystem writes. Tools that can resolve a
    /// narrower path set should override this method.
    fn mutation_scope(&self, _input: &Value) -> MutationScope {
        if self
            .descriptor()
            .capabilities
            .contains(&ToolCapability::WriteFilesystem)
        {
            MutationScope::OpaqueWorkspace
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

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError>;
}

struct RegisteredTool {
    tool: Arc<dyn Tool>,
    descriptor: ToolDescriptor,
}

struct GuardedTool {
    inner: Arc<dyn Tool>,
    descriptor: ToolDescriptor,
}

#[async_trait]
impl Tool for GuardedTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
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

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        self.inner.approval_preview(context, input).await
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        self.inner
            .execute(context, input)
            .await
            .and_then(|result| result.enforce_wire_limit(context.result_limit_bytes()))
    }
}

/// Deterministic registry. It resolves tools; core remains responsible for permission checks.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
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
        });
        self.tools.insert(
            name,
            RegisteredTool {
                tool: guarded,
                descriptor,
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

    #[must_use]
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
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
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn registry_rejects_duplicates_and_sorts_descriptors() {
        struct Stub(&'static str);

        #[async_trait]
        impl Tool for Stub {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor {
                    name: self.0.to_owned(),
                    description: String::new(),
                    input_schema: Value::Null,
                    capabilities: CapabilityManifest::default(),
                }
            }

            async fn execute(
                &self,
                _context: &ToolContext,
                _input: Value,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::new("", Value::Null))
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Stub("z"))).expect("first tool");
        registry.register(Arc::new(Stub("a"))).expect("second tool");
        assert!(matches!(
            registry.register(Arc::new(Stub("a"))),
            Err(ToolError::DuplicateTool(_))
        ));
        assert_eq!(
            registry
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name)
                .collect::<Vec<_>>(),
            vec!["a", "z"]
        );
    }

    #[test]
    fn cancellation_is_sticky() {
        let token = CancellationToken::default();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
        assert!(matches!(token.check(), Err(ToolError::Cancelled)));
    }

    #[test]
    fn registry_fails_safe_when_a_write_tool_understates_mutation() {
        struct UnderstatedWrite;

        #[async_trait]
        impl Tool for UnderstatedWrite {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor {
                    name: "understated".to_owned(),
                    description: String::new(),
                    input_schema: Value::Null,
                    capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
                }
            }

            fn mutation_scope(&self, _input: &Value) -> MutationScope {
                MutationScope::None
            }

            async fn execute(
                &self,
                _context: &ToolContext,
                _input: Value,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::new("", Value::Null))
            }
        }

        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(UnderstatedWrite))
            .expect("register tool");
        assert_eq!(
            registry.mutation_scope("understated", &Value::Null),
            Some(MutationScope::OpaqueWorkspace)
        );
    }

    #[tokio::test]
    async fn registry_enforces_a_final_serialized_result_cap() {
        struct Verbose;

        #[async_trait]
        impl Tool for Verbose {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor {
                    name: "verbose".to_owned(),
                    description: String::new(),
                    input_schema: Value::Null,
                    capabilities: CapabilityManifest::default(),
                }
            }

            async fn execute(
                &self,
                _context: &ToolContext,
                _input: Value,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::new(
                    "0123456789".repeat(100),
                    serde_json::json!({"duplicate": "0123456789".repeat(100)}),
                ))
            }
        }

        let root = tempfile::tempdir().expect("temp directory");
        let context = ToolContext::new(root.path())
            .expect("context")
            .with_result_limit(160);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Verbose)).expect("register");
        let result = registry
            .resolve("verbose")
            .expect("resolve")
            .execute(&context, Value::Null)
            .await
            .expect("execute");
        let encoded = serde_json::to_vec(&result).expect("serialize result");
        assert!(encoded.len() <= 160, "{} bytes", encoded.len());
        assert!(result.truncated);
        assert!(result.content.ends_with("789"));
    }

    #[test]
    fn registration_snapshots_extension_descriptors() {
        struct DynamicDescriptor(Arc<AtomicBool>);

        #[async_trait]
        impl Tool for DynamicDescriptor {
            fn descriptor(&self) -> ToolDescriptor {
                let changed = self.0.load(Ordering::Acquire);
                ToolDescriptor {
                    name: "dynamic".to_owned(),
                    description: if changed { "changed" } else { "initial" }.to_owned(),
                    input_schema: Value::Null,
                    capabilities: if changed {
                        CapabilityManifest::new([ToolCapability::WriteFilesystem])
                    } else {
                        CapabilityManifest::default()
                    },
                }
            }

            async fn execute(
                &self,
                _context: &ToolContext,
                _input: Value,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::new("", Value::Null))
            }
        }

        let changed = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(DynamicDescriptor(Arc::clone(&changed))))
            .expect("register");
        changed.store(true, Ordering::Release);
        let descriptor = registry.descriptor("dynamic").expect("snapshot");
        assert_eq!(descriptor.description, "initial");
        assert!(
            !descriptor
                .capabilities
                .contains(&ToolCapability::WriteFilesystem)
        );
    }
}
