#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use rw_intel::{IntelError, SymbolIndex};
use rw_types::ToolCapability;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::registry::{
    ApprovalPreview, CandidateLocation, CapabilityManifest, MutationScope, Tool, ToolContext,
    ToolDescriptor, ToolError, ToolLimits, ToolResult, input_schema, parse_input,
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    pub path: PathBuf,
    #[serde(default = "default_line")]
    pub start_line: usize,
    pub line_count: Option<usize>,
}

const fn default_line() -> usize {
    1
}

#[derive(Clone, Debug)]
pub struct ReadTool {
    limits: ToolLimits,
}

impl ReadTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self { limits }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        descriptor::<ReadInput>(
            "read",
            "Read a UTF-8 workspace file with optional line bounds.",
            [ToolCapability::ReadFilesystem],
        )
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: ReadInput = parse_input(input)?;
        if input.start_line == 0 {
            return Err(ToolError::InvalidInput(
                "start_line is one-based and must be positive".to_owned(),
            ));
        }
        let path = context.resolve_existing(&input.path)?;
        let bytes = read_capped(
            context,
            &path,
            self.limits.max_read_bytes.min(self.limits.max_result_bytes),
        )
        .await?;
        context.cancellation.check()?;
        let text = String::from_utf8(bytes)
            .map_err(|_| ToolError::InvalidInput("read only supports UTF-8 files".to_owned()))?;
        let total_lines = text.lines().count();
        let take = input.line_count.unwrap_or(usize::MAX);
        let selected = text
            .lines()
            .skip(input.start_line.saturating_sub(1))
            .take(take)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult::new(
            selected,
            json!({
                "path": context.relative_display(&path),
                "start_line": input.start_line,
                "total_lines": total_lines,
                "bytes": text.len(),
            }),
        ))
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteInput {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Clone)]
pub struct WriteTool {
    limits: ToolLimits,
    symbol_index: Option<Arc<SymbolIndex>>,
}

impl WriteTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            symbol_index: None,
        }
    }

    #[must_use]
    pub fn with_symbol_index(mut self, index: Arc<SymbolIndex>) -> Self {
        self.symbol_index = Some(index);
        self
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn descriptor(&self) -> ToolDescriptor {
        descriptor::<WriteInput>(
            "write",
            "Atomically write a UTF-8 workspace file.",
            [
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
            ],
        )
    }

    fn mutation_scope(&self, input: &Value) -> MutationScope {
        mutation_path(input)
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        let input: WriteInput = parse_input(input.clone())?;
        ensure_size(input.content.len(), self.limits.max_write_bytes)?;
        let path = context.resolve_writable(&input.path)?;
        let before = if path.exists() {
            Some(read_capped(context, &path, self.limits.max_write_bytes).await?)
        } else {
            None
        };
        Ok(Some(ApprovalPreview {
            path: context.relative_display(&path),
            before,
            after: input.content.into_bytes(),
        }))
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: WriteInput = parse_input(input)?;
        ensure_size(input.content.len(), self.limits.max_write_bytes)?;
        let path = context.resolve_writable(&input.path)?;
        atomic_write(
            context,
            &path,
            input.content.as_bytes(),
            &context.cancellation,
        )
        .await?;
        update_symbol_index(self.symbol_index.as_deref(), context, &path, &input.content);
        Ok(ToolResult::new(
            format!("wrote {} bytes", input.content.len()),
            json!({"path": context.relative_display(&path), "bytes": input.content.len()}),
        ))
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditInput {
    pub path: PathBuf,
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditOperation {
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MultiEditInput {
    pub path: PathBuf,
    pub edits: Vec<EditOperation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MatchMode {
    Exact,
    WhitespaceNormalized,
}

#[derive(Clone)]
pub struct EditTool {
    limits: ToolLimits,
    symbol_index: Option<Arc<SymbolIndex>>,
}

impl EditTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            symbol_index: None,
        }
    }

    #[must_use]
    pub fn with_symbol_index(mut self, index: Arc<SymbolIndex>) -> Self {
        self.symbol_index = Some(index);
        self
    }
}

#[async_trait]
impl Tool for EditTool {
    fn descriptor(&self) -> ToolDescriptor {
        descriptor::<EditInput>(
            "edit",
            "Replace one unambiguous span, trying exact text before whitespace-normalized matching.",
            [
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
            ],
        )
    }

    fn mutation_scope(&self, input: &Value) -> MutationScope {
        mutation_path(input)
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        let input: EditInput = parse_input(input.clone())?;
        let path = context.resolve_existing(&input.path)?;
        let before = read_capped(context, &path, self.limits.max_write_bytes).await?;
        let source = String::from_utf8(before.clone())
            .map_err(|_| ToolError::InvalidInput("edit only supports UTF-8 files".to_owned()))?;
        let (after, _) = apply_edit(&source, &input.old, &input.new)?;
        ensure_size(after.len(), self.limits.max_write_bytes)?;
        Ok(Some(ApprovalPreview {
            path: context.relative_display(&path),
            before: Some(before),
            after: after.into_bytes(),
        }))
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: EditInput = parse_input(input)?;
        let path = context.resolve_existing(&input.path)?;
        let bytes = read_capped(context, &path, self.limits.max_write_bytes).await?;
        let source = String::from_utf8(bytes)
            .map_err(|_| ToolError::InvalidInput("edit only supports UTF-8 files".to_owned()))?;
        let (edited, mode) = apply_edit(&source, &input.old, &input.new)?;
        ensure_size(edited.len(), self.limits.max_write_bytes)?;
        atomic_write(context, &path, edited.as_bytes(), &context.cancellation).await?;
        update_symbol_index(self.symbol_index.as_deref(), context, &path, &edited);
        Ok(ToolResult::new(
            "applied 1 edit",
            json!({"path": context.relative_display(&path), "match_mode": mode}),
        ))
    }
}

#[derive(Clone)]
pub struct MultiEditTool {
    limits: ToolLimits,
    symbol_index: Option<Arc<SymbolIndex>>,
}

impl MultiEditTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            symbol_index: None,
        }
    }

    #[must_use]
    pub fn with_symbol_index(mut self, index: Arc<SymbolIndex>) -> Self {
        self.symbol_index = Some(index);
        self
    }
}

#[async_trait]
impl Tool for MultiEditTool {
    fn descriptor(&self) -> ToolDescriptor {
        descriptor::<MultiEditInput>(
            "multi_edit",
            "Apply an ordered edit batch atomically; no file is written unless every edit matches.",
            [
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
            ],
        )
    }

    fn mutation_scope(&self, input: &Value) -> MutationScope {
        mutation_path(input)
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        let input: MultiEditInput = parse_input(input.clone())?;
        if input.edits.is_empty() {
            return Err(ToolError::InvalidInput(
                "edits must contain at least one operation".to_owned(),
            ));
        }
        let path = context.resolve_existing(&input.path)?;
        let before = read_capped(context, &path, self.limits.max_write_bytes).await?;
        let mut source = String::from_utf8(before.clone()).map_err(|_| {
            ToolError::InvalidInput("multi_edit only supports UTF-8 files".to_owned())
        })?;
        for edit in input.edits {
            let (next, _) = apply_edit(&source, &edit.old, &edit.new)?;
            ensure_size(next.len(), self.limits.max_write_bytes)?;
            source = next;
        }
        Ok(Some(ApprovalPreview {
            path: context.relative_display(&path),
            before: Some(before),
            after: source.into_bytes(),
        }))
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: MultiEditInput = parse_input(input)?;
        if input.edits.is_empty() {
            return Err(ToolError::InvalidInput(
                "edits must contain at least one operation".to_owned(),
            ));
        }
        let path = context.resolve_existing(&input.path)?;
        let bytes = read_capped(context, &path, self.limits.max_write_bytes).await?;
        let mut source = String::from_utf8(bytes).map_err(|_| {
            ToolError::InvalidInput("multi_edit only supports UTF-8 files".to_owned())
        })?;
        let mut modes = Vec::with_capacity(input.edits.len());
        for edit in &input.edits {
            context.cancellation.check()?;
            let (next, mode) = apply_edit(&source, &edit.old, &edit.new)?;
            ensure_size(next.len(), self.limits.max_write_bytes)?;
            source = next;
            modes.push(mode);
        }
        atomic_write(context, &path, source.as_bytes(), &context.cancellation).await?;
        update_symbol_index(self.symbol_index.as_deref(), context, &path, &source);
        Ok(ToolResult::new(
            format!("applied {} edits", modes.len()),
            json!({
                "path": context.relative_display(&path),
                "edits": modes.len(),
                "match_modes": modes,
            }),
        ))
    }
}

fn descriptor<T: JsonSchema>(
    name: &str,
    description: &str,
    capabilities: impl IntoIterator<Item = ToolCapability>,
) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: input_schema::<T>(),
        capabilities: CapabilityManifest::new(capabilities),
    }
}

fn mutation_path(input: &Value) -> MutationScope {
    let Some(path) = input
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        return MutationScope::OpaqueWorkspace;
    };
    if path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        MutationScope::OpaqueWorkspace
    } else {
        MutationScope::Paths(vec![path])
    }
}

async fn read_capped(
    context: &ToolContext,
    path: &std::path::Path,
    limit: usize,
) -> Result<Vec<u8>, ToolError> {
    #[cfg(unix)]
    let file = {
        let (parent, file_name) = context.secure_parent(path)?;
        let descriptor = rustix::fs::openat(
            parent,
            file_name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| ToolError::Io {
            operation: "open file without following links",
            path: path.to_path_buf(),
            source: source.into(),
        })?;
        let file = std::fs::File::from(descriptor);
        if !file
            .metadata()
            .map_err(|source| ToolError::Io {
                operation: "inspect opened file",
                path: path.to_path_buf(),
                source,
            })?
            .file_type()
            .is_file()
        {
            return Err(ToolError::InvalidInput(format!(
                "{} is not a regular file",
                context.relative_display(path).display()
            )));
        }
        tokio::fs::File::from_std(file)
    };
    #[cfg(not(unix))]
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|source| ToolError::Io {
            operation: "open file",
            path: path.to_path_buf(),
            source,
        })?;
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| ToolError::Io {
            operation: "read file",
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > limit {
        return Err(ToolError::SizeLimit { limit });
    }
    Ok(bytes)
}

fn ensure_size(size: usize, limit: usize) -> Result<(), ToolError> {
    if size > limit {
        Err(ToolError::SizeLimit { limit })
    } else {
        Ok(())
    }
}

fn update_symbol_index(
    index: Option<&SymbolIndex>,
    context: &ToolContext,
    path: &std::path::Path,
    source: &str,
) {
    let Some(index) = index else {
        return;
    };
    let relative = context.relative_display(path);
    match index.update_source(&relative, source) {
        Ok(_) | Err(IntelError::UnsupportedLanguage(_)) => {}
        Err(_) => {
            // The file mutation is already committed. Indexing is advisory: remove stale data and
            // let the next watcher/startup pass retry rather than reporting a false write failure.
            let _ = index.remove_path(relative);
        }
    }
}

async fn atomic_write(
    context: &ToolContext,
    path: &std::path::Path,
    payload: &[u8],
    cancellation: &crate::CancellationToken,
) -> Result<(), ToolError> {
    #[cfg(unix)]
    {
        return atomic_write_unix(context, path, payload, cancellation).await;
    }
    #[cfg(not(unix))]
    {
        atomic_write_portable(path, payload, cancellation).await
    }
}

#[cfg(unix)]
async fn atomic_write_unix(
    context: &ToolContext,
    path: &std::path::Path,
    payload: &[u8],
    cancellation: &crate::CancellationToken,
) -> Result<(), ToolError> {
    cancellation.check()?;
    let (parent, file_name) = context.secure_parent(path)?;
    let existing_permissions =
        match rustix::fs::statat(&parent, &file_name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
                    return Err(ToolError::InvalidInput(format!(
                        "{} is not a regular file",
                        context.relative_display(path).display()
                    )));
                }
                Some(std::fs::Permissions::from_mode(
                    u32::from(stat.st_mode) & 0o7777,
                ))
            }
            Err(rustix::io::Errno::NOENT) => None,
            Err(source) => {
                return Err(ToolError::Io {
                    operation: "inspect existing file without following links",
                    path: path.to_path_buf(),
                    source: source.into(),
                });
            }
        };
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = format!(
        ".{}.rottweiler.{}.{sequence}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    );
    let descriptor = rustix::fs::openat(
        &parent,
        temporary.as_str(),
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::from_bits_truncate(0o666),
    )
    .map_err(|source| ToolError::Io {
        operation: "create temporary file",
        path: path.with_file_name(&temporary),
        source: source.into(),
    })?;
    let mut file = tokio::fs::File::from_std(std::fs::File::from(descriptor));
    let result = async {
        file.write_all(payload)
            .await
            .map_err(|source| ToolError::Io {
                operation: "write temporary file",
                path: path.with_file_name(&temporary),
                source,
            })?;
        file.flush().await.map_err(|source| ToolError::Io {
            operation: "flush temporary file",
            path: path.with_file_name(&temporary),
            source,
        })?;
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)
                .await
                .map_err(|source| ToolError::Io {
                    operation: "preserve file permissions",
                    path: path.with_file_name(&temporary),
                    source,
                })?;
        }
        file.sync_all().await.map_err(|source| ToolError::Io {
            operation: "synchronize temporary file",
            path: path.with_file_name(&temporary),
            source,
        })?;
        cancellation.check()?;
        drop(file);
        rustix::fs::renameat(&parent, temporary.as_str(), &parent, &file_name).map_err(
            |source| ToolError::Io {
                operation: "replace file",
                path: path.to_path_buf(),
                source: source.into(),
            },
        )?;
        rustix::fs::fsync(&parent).map_err(|source| ToolError::Io {
            operation: "synchronize parent directory",
            path: path
                .parent()
                .map_or_else(|| path.to_path_buf(), std::path::Path::to_path_buf),
            source: source.into(),
        })
    }
    .await;
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&parent, temporary.as_str(), rustix::fs::AtFlags::empty());
    }
    result
}

#[cfg(not(unix))]
async fn atomic_write_portable(
    path: &std::path::Path,
    content: &[u8],
    cancellation: &crate::CancellationToken,
) -> Result<(), ToolError> {
    cancellation.check()?;
    let existing_permissions = tokio::fs::metadata(path)
        .await
        .ok()
        .map(|metadata| metadata.permissions());
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!(
        ".{file_name}.rottweiler.{}.{sequence}.tmp",
        std::process::id()
    ));
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    let mut file = options
        .open(&temporary)
        .await
        .map_err(|source| ToolError::Io {
            operation: "create temporary file",
            path: temporary.clone(),
            source,
        })?;
    let result = async {
        file.write_all(content)
            .await
            .map_err(|source| ToolError::Io {
                operation: "write temporary file",
                path: temporary.clone(),
                source,
            })?;
        file.flush().await.map_err(|source| ToolError::Io {
            operation: "flush temporary file",
            path: temporary.clone(),
            source,
        })?;
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)
                .await
                .map_err(|source| ToolError::Io {
                    operation: "preserve file permissions",
                    path: temporary.clone(),
                    source,
                })?;
        }
        file.sync_all().await.map_err(|source| ToolError::Io {
            operation: "synchronize temporary file",
            path: temporary.clone(),
            source,
        })?;
        cancellation.check()?;
        drop(file);
        tokio::fs::rename(&temporary, path)
            .await
            .map_err(|source| ToolError::Io {
                operation: "replace file",
                path: path.to_path_buf(),
                source,
            })
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

fn apply_edit(source: &str, old: &str, new: &str) -> Result<(String, MatchMode), ToolError> {
    if old.is_empty() {
        return Err(ToolError::InvalidInput(
            "old text must not be empty".to_owned(),
        ));
    }
    let exact = overlapping_match_starts(source, old);
    match exact.as_slice() {
        [start] => {
            let mut result = source.to_owned();
            result.replace_range(*start..(*start + old.len()), new);
            return Ok((result, MatchMode::Exact));
        }
        [] => {}
        starts => {
            return Err(ToolError::AmbiguousEdit {
                candidates: starts
                    .iter()
                    .map(|start| candidate_location(source, *start))
                    .collect(),
            });
        }
    }

    let old_tokens = whitespace_tokens(old);
    if old_tokens.is_empty() {
        return Err(ToolError::EditNotFound);
    }
    let source_tokens = whitespace_tokens(source);
    let candidates: Vec<(usize, usize)> = source_tokens
        .windows(old_tokens.len())
        .filter(|window| {
            window
                .iter()
                .zip(&old_tokens)
                .all(|(left, right)| left.text == right.text)
        })
        .map(|window| {
            let first = &window[0];
            let last = &window[window.len() - 1];
            (first.start, last.end)
        })
        .collect();
    match candidates.as_slice() {
        [(start, end)] => {
            let mut result = source.to_owned();
            result.replace_range(*start..*end, new);
            Ok((result, MatchMode::WhitespaceNormalized))
        }
        [] => Err(ToolError::EditNotFound),
        locations => Err(ToolError::AmbiguousEdit {
            candidates: locations
                .iter()
                .map(|(start, _)| candidate_location(source, *start))
                .collect(),
        }),
    }
}

fn overlapping_match_starts(source: &str, needle: &str) -> Vec<usize> {
    source
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(source.len()))
        .filter(|start| source[*start..].starts_with(needle))
        .collect()
}

struct WhitespaceToken<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn whitespace_tokens(source: &str) -> Vec<WhitespaceToken<'_>> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, character) in source.char_indices() {
        if character.is_whitespace() {
            if let Some(token_start) = start.take() {
                tokens.push(WhitespaceToken {
                    text: &source[token_start..index],
                    start: token_start,
                    end: index,
                });
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(token_start) = start {
        tokens.push(WhitespaceToken {
            text: &source[token_start..],
            start: token_start,
            end: source.len(),
        });
    }
    tokens
}

fn candidate_location(source: &str, byte: usize) -> CandidateLocation {
    let prefix = &source[..byte];
    CandidateLocation {
        line: prefix.bytes().filter(|value| *value == b'\n').count() + 1,
        column: prefix
            .rfind('\n')
            .map_or(prefix.len() + 1, |position| prefix.len() - position),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn added_root_is_writable_while_its_parent_remains_blocked() {
        let root = tempfile::tempdir().expect("root");
        let primary = root.path().join("primary");
        let added = root.path().join("added");
        std::fs::create_dir_all(&primary).expect("primary");
        std::fs::create_dir_all(&added).expect("added");
        std::fs::write(root.path().join("parent.txt"), "blocked").expect("parent fixture");
        let context = ToolContext::from_workspace_roots([&primary, &added]).expect("context");
        let write = WriteTool::new(ToolLimits::default());

        write
            .execute(
                &context,
                json!({"path": "@root/1/created.txt", "content": "from added root"}),
            )
            .await
            .expect("write added root");
        assert_eq!(
            std::fs::read_to_string(added.join("created.txt")).expect("created"),
            "from added root"
        );
        write
            .execute(
                &context,
                json!({"path": "../added/relative.txt", "content": "relative sibling"}),
            )
            .await
            .expect("relative added root");
        assert!(matches!(
            write
                .execute(
                    &context,
                    json!({"path": "../parent.txt", "content": "escape"}),
                )
                .await,
            Err(ToolError::PathEscape(_))
        ));
        assert!(matches!(
            write
                .execute(
                    &context,
                    json!({"path": "@root/1/../parent.txt", "content": "escape"}),
                )
                .await,
            Err(ToolError::PathEscape(_))
        ));
        assert_eq!(
            std::fs::read_to_string(root.path().join("parent.txt")).expect("parent preserved"),
            "blocked"
        );

        let nested = primary.join("nested");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::write(nested.join("owned.txt"), "nested").expect("nested fixture");
        let nested_context =
            ToolContext::from_workspace_roots([&primary, &nested]).expect("nested context");
        let nested_file =
            std::fs::canonicalize(nested.join("owned.txt")).expect("canonical nested");
        assert_eq!(
            nested_context.relative_display(&nested_file),
            PathBuf::from("@root/1/owned.txt")
        );
    }

    #[test]
    fn edit_is_exact_first_then_normalized_and_never_guesses() {
        let (exact, mode) = apply_edit("a  b\na b", "a  b", "x").expect("exact");
        assert_eq!(exact, "x\na b");
        assert_eq!(mode, MatchMode::Exact);

        let (normalized, mode) =
            apply_edit("before\na   b\nafter", "a b", "x").expect("normalized fallback");
        assert_eq!(normalized, "before\nx\nafter");
        assert_eq!(mode, MatchMode::WhitespaceNormalized);

        let error = apply_edit("a  b\nother\na\tb", "a b", "x").expect_err("ambiguous");
        assert!(matches!(
            error,
            ToolError::AmbiguousEdit { ref candidates } if candidates.len() == 2
        ));
        assert!(matches!(
            apply_edit("aaa", "aa", "x"),
            Err(ToolError::AmbiguousEdit { ref candidates }) if candidates.len() == 2
        ));

        assert_eq!(
            WriteTool::new(ToolLimits::default())
                .mutation_scope(&json!({"path": "src/lib.rs", "content": ""})),
            MutationScope::Paths(vec![PathBuf::from("src/lib.rs")])
        );
        assert_eq!(
            EditTool::new(ToolLimits::default()).mutation_scope(&Value::Null),
            MutationScope::OpaqueWorkspace
        );
        for unsafe_path in ["../outside.rs", "/tmp/outside.rs", "."] {
            assert_eq!(
                WriteTool::new(ToolLimits::default())
                    .mutation_scope(&json!({"path": unsafe_path, "content": ""})),
                MutationScope::OpaqueWorkspace
            );
        }
    }

    #[tokio::test]
    async fn multi_edit_does_not_write_a_partial_batch() {
        let root = tempdir().expect("temp directory");
        fs::write(root.path().join("sample.txt"), "one two three").expect("fixture");
        let context = ToolContext::new(root.path()).expect("context");
        let tool = MultiEditTool::new(ToolLimits::default());
        let error = tool
            .execute(
                &context,
                json!({
                    "path": "sample.txt",
                    "edits": [
                        {"old": "one", "new": "ONE"},
                        {"old": "missing", "new": "MISSING"}
                    ]
                }),
            )
            .await
            .expect_err("second edit fails");
        assert!(matches!(error, ToolError::EditNotFound));
        assert_eq!(
            fs::read_to_string(root.path().join("sample.txt")).expect("unchanged fixture"),
            "one two three"
        );
    }

    #[tokio::test]
    async fn read_and_write_apply_content_caps() {
        let root = tempdir().expect("temp directory");
        fs::write(root.path().join("large.txt"), "0123456789").expect("fixture");
        let context = ToolContext::new(root.path()).expect("context");
        let limits = ToolLimits {
            max_read_bytes: 4,
            max_write_bytes: 4,
            ..ToolLimits::default()
        };
        assert!(matches!(
            ReadTool::new(limits)
                .execute(&context, json!({"path": "large.txt"}))
                .await,
            Err(ToolError::SizeLimit { limit: 4 })
        ));
        assert!(matches!(
            WriteTool::new(limits)
                .execute(&context, json!({"path": "new.txt", "content": "12345"}))
                .await,
            Err(ToolError::SizeLimit { limit: 4 })
        ));
    }

    #[tokio::test]
    async fn committed_writes_fail_open_and_remove_stale_symbols_when_indexing_rejects_content() {
        let root = tempdir().expect("temp directory");
        let path = root.path().join("lib.rs");
        fs::write(&path, "struct Old;").expect("fixture");
        let index = Arc::new(SymbolIndex::new(root.path()).expect("index").with_limits(
            rw_intel::IndexLimits {
                max_file_bytes: 32,
                ..rw_intel::IndexLimits::default()
            },
        ));
        index
            .update_source("lib.rs", "struct Old;")
            .expect("old symbol");
        let replacement = "pub struct NewName;\n".repeat(4);
        let context = ToolContext::new(root.path()).expect("context");
        WriteTool::new(ToolLimits::default())
            .with_symbol_index(Arc::clone(&index))
            .execute(&context, json!({"path": "lib.rs", "content": replacement}))
            .await
            .expect("committed write remains successful");
        assert_eq!(
            fs::read_to_string(&path).expect("committed content"),
            replacement
        );
        assert!(
            index
                .symbols_for_file("lib.rs")
                .expect("symbols")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_preserves_executable_mode_and_updates_the_symbol_index() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().expect("temp directory");
        let path = root.path().join("tool.rs");
        fs::write(&path, "fn before() {}\n").expect("fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("permissions");
        let index = Arc::new(SymbolIndex::new(root.path()).expect("index"));
        let context = ToolContext::new(root.path()).expect("context");
        EditTool::new(ToolLimits::default())
            .with_symbol_index(Arc::clone(&index))
            .execute(
                &context,
                json!({"path": "tool.rs", "old": "before", "new": "after"}),
            )
            .await
            .expect("edit");
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o755
        );
        let symbols = index.symbols_for_file("tool.rs").expect("indexed symbols");
        assert!(symbols.iter().any(|symbol| symbol.name == "after"));
        assert!(!symbols.iter().any(|symbol| symbol.name == "before"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_file_tools_reject_symlinks_escaping_the_workspace() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        fs::write(outside.path().join("secret.txt"), "secret").expect("outside file");
        symlink(outside.path(), root.path().join("escape")).expect("symlink");
        let context = ToolContext::new(root.path()).expect("context");
        assert!(matches!(
            ReadTool::new(ToolLimits::default())
                .execute(&context, json!({"path": "escape/secret.txt"}))
                .await,
            Err(ToolError::PathEscape(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn special_files_are_rejected_without_blocking_and_write_only_mode_is_preserved() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().expect("workspace");
        let fifo = root.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("run mkfifo")
                .success()
        );
        let context = ToolContext::new(root.path()).expect("context");
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ReadTool::new(ToolLimits::default()).execute(&context, json!({"path": "pipe"})),
        )
        .await
        .expect("read must not block");
        assert!(matches!(read, Err(ToolError::InvalidInput(_))));
        let write = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            WriteTool::new(ToolLimits::default())
                .execute(&context, json!({"path": "pipe", "content": "replacement"})),
        )
        .await
        .expect("write must not block");
        assert!(matches!(write, Err(ToolError::InvalidInput(_))));

        let write_only = root.path().join("write-only.txt");
        fs::write(&write_only, "old").expect("write-only fixture");
        fs::set_permissions(&write_only, fs::Permissions::from_mode(0o200))
            .expect("write-only permissions");
        WriteTool::new(ToolLimits::default())
            .execute(
                &context,
                json!({"path": "write-only.txt", "content": "new"}),
            )
            .await
            .expect("write-only replacement");
        assert_eq!(
            fs::metadata(&write_only)
                .expect("write-only metadata")
                .permissions()
                .mode()
                & 0o777,
            0o200
        );
        fs::set_permissions(&write_only, fs::Permissions::from_mode(0o600))
            .expect("restore readable mode");
        assert_eq!(
            fs::read_to_string(&write_only).expect("replaced content"),
            "new"
        );
    }
}
