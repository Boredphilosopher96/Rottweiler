use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

use async_trait::async_trait;
use rw_intel::{
    IndexBudget, IndexLimits, IntelError, Language, Symbol, SymbolIndex, SymbolQuery, SymbolRole,
};
use rw_types::ToolCapability;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::registry::{
    CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError, ToolLimits, ToolResult,
    input_schema, parse_input,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolsInput {
    pub pattern: String,
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub roles: Vec<SymbolRole>,
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub languages: Vec<Language>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

const fn default_limit() -> usize {
    100
}

#[derive(Clone)]
pub struct SymbolsTool {
    index: Arc<WorkspaceSymbolIndex>,
    limits: ToolLimits,
}

impl SymbolsTool {
    #[must_use]
    pub fn new(index: Arc<WorkspaceSymbolIndex>, limits: ToolLimits) -> Self {
        Self { index, limits }
    }
}

/// Runtime-owned index sharing. Canonical roots and trust scope partition data.
pub struct WorkspaceIndexPool {
    budget: Arc<IndexBudget>,
    indexes: Mutex<BTreeMap<(PathBuf, bool), Weak<SymbolIndex>>>,
}

impl Default for WorkspaceIndexPool {
    fn default() -> Self {
        Self {
            budget: Arc::new(IndexBudget::new(IndexLimits::default().max_retained_bytes)),
            indexes: Mutex::new(BTreeMap::new()),
        }
    }
}

impl WorkspaceIndexPool {
    /// Shares a root only within the same trust scope and canonical worktree.
    ///
    /// # Errors
    /// Returns filesystem, lock or aggregate-capacity failures.
    pub fn workspace(
        &self,
        roots: &[PathBuf],
        trusted: &[bool],
    ) -> Result<WorkspaceSymbolIndex, IntelError> {
        if roots.len() != trusted.len() {
            return Err(IntelError::Capacity);
        }
        let mut entries = self.indexes.lock().map_err(|_| IntelError::LockPoisoned)?;
        entries.retain(|_, index| index.strong_count() > 0);
        let mut indexes = Vec::with_capacity(roots.len());
        for (root, trusted) in roots.iter().zip(trusted) {
            let canonical = std::fs::canonicalize(root).map_err(|source| IntelError::Io {
                path: root.clone(),
                source,
            })?;
            let key = (canonical, *trusted);
            let index = if let Some(index) = entries.get(&key).and_then(Weak::upgrade) {
                index
            } else {
                if entries.len() >= 128 {
                    return Err(IntelError::Capacity);
                }
                let index =
                    Arc::new(SymbolIndex::new(&key.0)?.with_budget(Arc::clone(&self.budget)));
                entries.insert(key, Arc::downgrade(&index));
                index
            };
            indexes.push(index);
        }
        Ok(WorkspaceSymbolIndex { indexes })
    }
}

/// Stable-index symbol aggregation across every workspace root.
pub struct WorkspaceSymbolIndex {
    indexes: Vec<Arc<SymbolIndex>>,
}

impl WorkspaceSymbolIndex {
    /// Creates bounded root indexes under one shared byte budget.
    ///
    /// # Errors
    ///
    /// Returns an indexing error when any root cannot be canonicalized.
    pub fn new(roots: impl IntoIterator<Item = impl AsRef<Path>>) -> Result<Self, IntelError> {
        Self::new_with_limits(roots, IndexLimits::default())
    }

    /// Creates root indexes with explicit resource limits.
    ///
    /// # Errors
    ///
    /// Returns an indexing error when any root cannot be canonicalized.
    pub fn new_with_limits(
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
        limits: IndexLimits,
    ) -> Result<Self, IntelError> {
        let budget = Arc::new(IndexBudget::new(limits.max_retained_bytes));
        let indexes = roots
            .into_iter()
            .map(|root| {
                SymbolIndex::new(root).map(|index| {
                    Arc::new(index.with_limits(limits).with_budget(Arc::clone(&budget)))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { indexes })
    }

    /// Indexes supported files under every root.
    ///
    /// # Errors
    ///
    /// Returns the first filesystem, parsing, or index-lock error.
    pub fn index_workspaces(&self) -> Result<(), IntelError> {
        for index in &self.indexes {
            index.index_workspace()?;
        }
        Ok(())
    }

    /// Reconciles externally changed files on the index owner's shared schedule.
    ///
    /// # Errors
    /// Returns filesystem, parse or admission errors.
    pub fn ensure_current(&self) -> Result<(), IntelError> {
        for index in &self.indexes {
            index.ensure_current()?;
        }
        Ok(())
    }

    /// Ordered per-root indexes used by the optional LSP facade. Added roots
    /// have the same index as their `@root/N` virtual path prefix.
    #[must_use]
    pub fn root_indexes(&self) -> &[Arc<SymbolIndex>] {
        &self.indexes
    }

    /// Updates one plain or `@root/N` virtual path.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid root routing or rejected source content.
    pub fn update_source(&self, path: impl AsRef<Path>, source: &str) -> Result<usize, IntelError> {
        let (index, relative) = self.route(path.as_ref())?;
        self.indexes[index].update_source(relative, source)
    }

    /// Removes one virtual path from its owning root index.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid root routing or a poisoned index lock.
    pub fn remove_path(&self, path: impl AsRef<Path>) -> Result<bool, IntelError> {
        let (index, relative) = self.route(path.as_ref())?;
        self.indexes[index].remove_path(relative)
    }

    /// Returns symbols for one virtual file path.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid root routing or a poisoned index lock.
    pub fn symbols_for_file(&self, path: impl AsRef<Path>) -> Result<Vec<Symbol>, IntelError> {
        let (index, relative) = self.route(path.as_ref())?;
        self.indexes[index].symbols_for_file(relative)
    }

    /// Queries every root and rewrites added-root results to `@root/N` paths.
    ///
    /// # Errors
    ///
    /// Returns an error when any root index cannot be queried.
    pub fn query(&self, query: &SymbolQuery) -> Result<Vec<Symbol>, IntelError> {
        let mut matches = Vec::new();
        for (root_index, index) in self.indexes.iter().enumerate() {
            let mut root_matches = index.query(query)?;
            if root_index > 0 {
                for symbol in &mut root_matches {
                    symbol.location.path = PathBuf::from("@root")
                        .join(root_index.to_string())
                        .join(&symbol.location.path);
                }
            }
            matches.extend(root_matches);
        }
        matches.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.location.path.cmp(&right.location.path))
                .then_with(|| left.location.line.cmp(&right.location.line))
                .then_with(|| left.location.column.cmp(&right.location.column))
        });
        matches.truncate(query.limit.clamp(1, 10_000));
        Ok(matches)
    }

    fn route(&self, path: &Path) -> Result<(usize, PathBuf), IntelError> {
        let mut components = path.components();
        let Some(first) = components.next() else {
            return Err(IntelError::PathEscape(path.to_path_buf()));
        };
        if matches!(first, Component::Normal(value) if value == "@root") {
            let index = match components.next() {
                Some(Component::Normal(value)) => value
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|index| *index > 0 && *index < self.indexes.len())
                    .ok_or_else(|| IntelError::PathEscape(path.to_path_buf()))?,
                _ => return Err(IntelError::PathEscape(path.to_path_buf())),
            };
            let relative = components.collect::<PathBuf>();
            if relative.as_os_str().is_empty() {
                return Err(IntelError::PathEscape(path.to_path_buf()));
            }
            Ok((index, relative))
        } else {
            Ok((0, path.to_path_buf()))
        }
    }
}

#[async_trait]
impl Tool for SymbolsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "symbols".to_owned(),
            description: "Search cached tree-sitter definitions and references across Rust, Python, and TypeScript."
                .to_owned(),
            input_schema: input_schema::<SymbolsInput>(),
            capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
        }
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: SymbolsInput = parse_input(input)?;
        if input.pattern.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "symbol pattern must not be empty".to_owned(),
            ));
        }
        let index = Arc::clone(&self.index);
        let query = SymbolQuery {
            pattern: input.pattern,
            roles: input.roles,
            languages: input.languages,
            limit: input.limit.min(self.limits.max_search_results),
        };
        let matches = tokio::task::spawn_blocking(move || index.query(&query))
            .await
            .map_err(|error| ToolError::Intelligence(error.to_string()))?
            .map_err(|error| ToolError::Intelligence(error.to_string()))?;
        context.cancellation.check()?;
        let mut retained = Vec::new();
        let mut model_text = String::new();
        let mut truncated = self
            .index
            .root_indexes()
            .iter()
            .any(|index| index.is_partial());
        for symbol in matches {
            let line = format!(
                "{}:{}:{} {:?} {:?} {}",
                symbol.location.path.display(),
                symbol.location.line,
                symbol.location.column,
                symbol.role,
                symbol.kind,
                symbol.name
            );
            let separator = usize::from(!model_text.is_empty());
            if model_text
                .len()
                .saturating_add(separator)
                .saturating_add(line.len())
                > self.limits.max_result_bytes
            {
                truncated = true;
                break;
            }
            if separator == 1 {
                model_text.push('\n');
            }
            model_text.push_str(&line);
            retained.push(symbol);
        }
        let mut result = ToolResult::new(
            model_text,
            json!({"matches": retained, "count": retained.len(), "truncated": truncated}),
        );
        result.truncated = truncated;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn runtime_pool_shares_matching_roots_and_partitions_trust_and_worktrees() {
        let root = tempdir().expect("root");
        let other = tempdir().expect("worktree");
        let pool = WorkspaceIndexPool::default();
        let roots = [root.path().to_path_buf()];
        let first = pool.workspace(&roots, &[true]).expect("workspace");
        let second = pool.workspace(&roots, &[true]).expect("same workspace");
        let untrusted = pool
            .workspace(&roots, &[false])
            .expect("untrusted workspace");
        let worktree = pool
            .workspace(&[other.path().to_path_buf()], &[true])
            .expect("worktree");
        assert!(Arc::ptr_eq(&first.indexes[0], &second.indexes[0]));
        assert!(!Arc::ptr_eq(&first.indexes[0], &untrusted.indexes[0]));
        assert!(!Arc::ptr_eq(&first.indexes[0], &worktree.indexes[0]));
        first
            .update_source("one.rs", "struct Shared;")
            .expect("source");
        assert!(
            !second
                .symbols_for_file("one.rs")
                .expect("symbols")
                .is_empty()
        );
        assert!(
            untrusted
                .symbols_for_file("one.rs")
                .expect("symbols")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn exposes_the_incremental_index_as_a_tool() {
        let root = tempdir().expect("temp directory");
        let index = Arc::new(WorkspaceSymbolIndex::new([root.path()]).expect("index"));
        index
            .update_source("lib.rs", "pub struct Rottweiler;")
            .expect("source");
        let context = ToolContext::new(root.path()).expect("context");
        let result = SymbolsTool::new(index, ToolLimits::default())
            .execute(
                &context,
                serde_json::json!({"pattern": "Rott", "roles": ["definition"]}),
            )
            .await
            .expect("symbols");
        assert_eq!(result.data["count"], 1);
        assert!(result.content.contains("Rottweiler"));
    }

    #[tokio::test]
    async fn aggregates_duplicate_symbols_with_stable_virtual_root_paths() {
        let primary = tempdir().expect("primary");
        let added = tempdir().expect("added");
        let index = Arc::new(
            WorkspaceSymbolIndex::new([primary.path(), added.path()]).expect("multi-root index"),
        );
        index
            .update_source("same.rs", "pub struct Shared;")
            .expect("primary source");
        index
            .update_source("@root/1/same.rs", "pub struct Shared;")
            .expect("added source");
        let context =
            ToolContext::from_workspace_roots([primary.path(), added.path()]).expect("context");
        let result = SymbolsTool::new(index.clone(), ToolLimits::default())
            .execute(&context, serde_json::json!({"pattern":"Shared"}))
            .await
            .expect("symbols");
        assert_eq!(result.data["count"], 2);
        assert!(result.content.contains("same.rs"));
        assert!(result.content.contains("@root/1/same.rs"));

        index
            .update_source("@root/1/same.rs", "pub struct AddedOnly;")
            .expect("added update");
        let result = SymbolsTool::new(index, ToolLimits::default())
            .execute(&context, serde_json::json!({"pattern":"AddedOnly"}))
            .await
            .expect("updated symbols");
        assert_eq!(result.data["count"], 1);
        assert!(result.content.contains("@root/1/same.rs"));
    }
}
