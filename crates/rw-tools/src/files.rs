mod presentation;
use presentation::{
    EDIT_PRESENTATION, MULTI_EDIT_PRESENTATION, READ_PRESENTATION, WRITE_PRESENTATION,
};

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rw_intel::IntelError;
use rw_types::ToolCapability;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::registry::{
    ApprovalPreview, CandidateLocation, CapabilityManifest, Tool, ToolBehavior, ToolContext,
    ToolDescriptor, ToolError, ToolLimits, ToolResult, input_schema, parse_input,
};
use crate::symbols::WorkspaceSymbolIndex;

mod io;
mod operations;
mod transaction;

use io::{atomic_write, atomic_write_if_unchanged, read_capped, read_capped_snapshot};
use operations::FileOperations;

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
    operations: FileOperations,
}

impl ReadTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            operations: FileOperations::new(),
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        self.operations.settle().await
    }

    fn descriptor(&self) -> ToolDescriptor {
        descriptor::<ReadInput>(
            "read",
            "Read a UTF-8 workspace file with optional line bounds.",
            [ToolCapability::ReadFilesystem],
        )
    }

    fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(vec![parse_input::<ReadInput>(input.clone())?.path])
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let tool = self.clone();
        self.operations
            .run(context.clone(), move |context, _transaction| {
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
                    tool.limits.max_read_bytes.min(tool.limits.max_result_bytes),
                )?;
                context.cancellation.check()?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    ToolError::InvalidInput("read only supports UTF-8 files".to_owned())
                })?;
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
                )
                .with_presentation(READ_PRESENTATION.plan()?))
            })
            .await
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
    operations: FileOperations,
    symbol_index: Option<Arc<WorkspaceSymbolIndex>>,
}

impl WriteTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            operations: FileOperations::new(),
            symbol_index: None,
        }
    }

    #[must_use]
    pub fn with_symbol_index(mut self, index: Arc<WorkspaceSymbolIndex>) -> Self {
        self.symbol_index = Some(index);
        self
    }
}

#[async_trait]
impl Tool for WriteTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        self.operations.settle().await
    }

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

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::FileMutation
    }

    fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(vec![parse_input::<WriteInput>(input.clone())?.path])
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        let tool = self.clone();
        let input = input.clone();
        self.operations
            .run(context.clone(), move |context, _transaction| {
                let input: WriteInput = parse_input(input.clone())?;
                ensure_size(input.content.len(), tool.limits.max_write_bytes)?;
                let path = context.resolve_writable(&input.path)?;
                let before = if path.exists() {
                    Some(read_capped(context, &path, tool.limits.max_write_bytes)?)
                } else {
                    None
                };
                Ok(Some(ApprovalPreview {
                    path: context.relative_display(&path),
                    before,
                    after: input.content.into_bytes(),
                }))
            })
            .await
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let tool = self.clone();
        self.operations
            .run(context.clone(), move |context, transaction| {
                context.cancellation.check()?;
                let input: WriteInput = parse_input(input)?;
                ensure_size(input.content.len(), tool.limits.max_write_bytes)?;
                let path = context.resolve_writable(&input.path)?;
                atomic_write(
                    transaction,
                    context,
                    &path,
                    input.content.as_bytes(),
                    &context.cancellation,
                )?;
                update_symbol_index(tool.symbol_index.as_deref(), context, &path, &input.content);
                Ok(ToolResult::new(
                    format!("wrote {} bytes", input.content.len()),
                    json!({"path": context.relative_display(&path), "bytes": input.content.len()}),
                )
                .with_presentation(WRITE_PRESENTATION.plan()?))
            })
            .await
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
    operations: FileOperations,
    symbol_index: Option<Arc<WorkspaceSymbolIndex>>,
}

impl EditTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            operations: FileOperations::new(),
            symbol_index: None,
        }
    }

    #[must_use]
    pub fn with_symbol_index(mut self, index: Arc<WorkspaceSymbolIndex>) -> Self {
        self.symbol_index = Some(index);
        self
    }
}

#[async_trait]
impl Tool for EditTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        self.operations.settle().await
    }

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

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::FileMutation
    }

    fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(vec![parse_input::<EditInput>(input.clone())?.path])
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        let tool = self.clone();
        let input = input.clone();
        self.operations
            .run(context.clone(), move |context, _transaction| {
                let input: EditInput = parse_input(input.clone())?;
                let path = context.resolve_existing(&input.path)?;
                let before = read_capped(context, &path, tool.limits.max_write_bytes)?;
                let source = String::from_utf8(before.clone()).map_err(|_| {
                    ToolError::InvalidInput("edit only supports UTF-8 files".to_owned())
                })?;
                let (after, _) = apply_edit(&source, &input.old, &input.new)?;
                ensure_size(after.len(), tool.limits.max_write_bytes)?;
                Ok(Some(ApprovalPreview {
                    path: context.relative_display(&path),
                    before: Some(before),
                    after: after.into_bytes(),
                }))
            })
            .await
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let tool = self.clone();
        self.operations
            .run(context.clone(), move |context, transaction| {
                context.cancellation.check()?;
                let input: EditInput = parse_input(input)?;
                let path = context.resolve_existing(&input.path)?;
                let snapshot = read_capped_snapshot(context, &path, tool.limits.max_write_bytes)?;
                let source = String::from_utf8(snapshot.bytes.clone()).map_err(|_| {
                    ToolError::InvalidInput("edit only supports UTF-8 files".to_owned())
                })?;
                let (edited, mode) = apply_edit(&source, &input.old, &input.new)?;
                ensure_size(edited.len(), tool.limits.max_write_bytes)?;
                atomic_write_if_unchanged(
                    transaction,
                    context,
                    &path,
                    edited.as_bytes(),
                    &snapshot,
                    &context.cancellation,
                )?;
                update_symbol_index(tool.symbol_index.as_deref(), context, &path, &edited);
                Ok(ToolResult::new(
                    "applied 1 edit",
                    json!({"path": context.relative_display(&path), "match_mode": mode}),
                )
                .with_presentation(EDIT_PRESENTATION.plan()?))
            })
            .await
    }
}

#[derive(Clone)]
pub struct MultiEditTool {
    limits: ToolLimits,
    operations: FileOperations,
    symbol_index: Option<Arc<WorkspaceSymbolIndex>>,
}

impl MultiEditTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            operations: FileOperations::new(),
            symbol_index: None,
        }
    }

    #[must_use]
    pub fn with_symbol_index(mut self, index: Arc<WorkspaceSymbolIndex>) -> Self {
        self.symbol_index = Some(index);
        self
    }
}

#[async_trait]
impl Tool for MultiEditTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        self.operations.settle().await
    }

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

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::FileMutation
    }

    fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(vec![parse_input::<MultiEditInput>(input.clone())?.path])
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<ApprovalPreview>, ToolError> {
        let tool = self.clone();
        let input = input.clone();
        self.operations
            .run(context.clone(), move |context, _transaction| {
                let input: MultiEditInput = parse_input(input.clone())?;
                if input.edits.is_empty() {
                    return Err(ToolError::InvalidInput(
                        "edits must contain at least one operation".to_owned(),
                    ));
                }
                let path = context.resolve_existing(&input.path)?;
                let before = read_capped(context, &path, tool.limits.max_write_bytes)?;
                let mut source = String::from_utf8(before.clone()).map_err(|_| {
                    ToolError::InvalidInput("multi_edit only supports UTF-8 files".to_owned())
                })?;
                for edit in input.edits {
                    let (next, _) = apply_edit(&source, &edit.old, &edit.new)?;
                    ensure_size(next.len(), tool.limits.max_write_bytes)?;
                    source = next;
                }
                Ok(Some(ApprovalPreview {
                    path: context.relative_display(&path),
                    before: Some(before),
                    after: source.into_bytes(),
                }))
            })
            .await
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let tool = self.clone();
        self.operations
            .run(context.clone(), move |context, transaction| {
                context.cancellation.check()?;
                let input: MultiEditInput = parse_input(input)?;
                if input.edits.is_empty() {
                    return Err(ToolError::InvalidInput(
                        "edits must contain at least one operation".to_owned(),
                    ));
                }
                let path = context.resolve_existing(&input.path)?;
                let snapshot = read_capped_snapshot(context, &path, tool.limits.max_write_bytes)?;
                let mut source = String::from_utf8(snapshot.bytes.clone()).map_err(|_| {
                    ToolError::InvalidInput("multi_edit only supports UTF-8 files".to_owned())
                })?;
                let mut modes = Vec::with_capacity(input.edits.len());
                for edit in &input.edits {
                    context.cancellation.check()?;
                    let (next, mode) = apply_edit(&source, &edit.old, &edit.new)?;
                    ensure_size(next.len(), tool.limits.max_write_bytes)?;
                    source = next;
                    modes.push(mode);
                }
                atomic_write_if_unchanged(
                    transaction,
                    context,
                    &path,
                    source.as_bytes(),
                    &snapshot,
                    &context.cancellation,
                )?;
                update_symbol_index(tool.symbol_index.as_deref(), context, &path, &source);
                Ok(ToolResult::new(
                    format!("applied {} edits", modes.len()),
                    json!({
                        "path": context.relative_display(&path),
                        "edits": modes.len(),
                        "match_modes": modes,
                    }),
                )
                .with_presentation(MULTI_EDIT_PRESENTATION.plan()?))
            })
            .await
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

fn ensure_size(size: usize, limit: usize) -> Result<(), ToolError> {
    if size > limit {
        Err(ToolError::SizeLimit { limit })
    } else {
        Ok(())
    }
}

fn update_symbol_index(
    index: Option<&WorkspaceSymbolIndex>,
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
mod tests;
