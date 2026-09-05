//! Bounded, local code intelligence backed by tree-sitter.

mod lsp;

pub use lsp::{
    CodeIntelligence, Diagnostic, DiagnosticSeverity, IntelligenceBackend, IntelligenceResult,
    Location, LspConfig, LspError, LspProcessHandle, LspProcessSpawner, LspServerConfig, Position,
    Range, RenameResult, SpawnedLspProcess, TextEdit, WorkspaceUriMapper,
};

use std::collections::{BTreeMap, BinaryHeap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tree_sitter::{Node, Parser, Tree};

/// Languages supported by the always-on syntax index.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Python,
    TypeScript,
}

impl Language {
    /// Detect a supported language from a source path.
    #[must_use]
    pub fn for_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("rs") => Some(Self::Rust),
            Some("py") => Some(Self::Python),
            Some("ts" | "tsx") => Some(Self::TypeScript),
            _ => None,
        }
    }

    fn parser_language(self, path: &Path) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript if path.extension().is_some_and(|extension| extension == "tsx") => {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            }
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        }
    }
}

/// Broad symbol kinds shared across supported grammars.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Module,
    Type,
    Constant,
    Variable,
    Identifier,
}

/// Whether a symbol occurrence introduces or refers to a name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolRole {
    Definition,
    Reference,
}

/// A precise source range, relative to the indexed workspace.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceLocation {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

/// A syntax-level symbol occurrence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub role: SymbolRole,
    pub language: Language,
    pub location: SourceLocation,
}

/// Bounded query over the in-memory index.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SymbolQuery {
    pub pattern: String,
    #[serde(default)]
    pub roles: Vec<SymbolRole>,
    #[serde(default)]
    pub languages: Vec<Language>,
    #[serde(default = "default_query_limit")]
    pub limit: usize,
}

const fn default_query_limit() -> usize {
    100
}

/// Resource limits for startup indexing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexLimits {
    pub max_files: usize,
    pub max_scan_entries: usize,
    pub max_file_bytes: usize,
    pub max_symbols_per_file: usize,
    pub max_retained_bytes: usize,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_scan_entries: 100_000,
            max_file_bytes: 2 * 1024 * 1024,
            max_symbols_per_file: 50_000,
            max_retained_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Summary of an indexing pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexStats {
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub symbols: usize,
}

/// Failures are local and never contain source contents.
#[derive(Debug, Error)]
pub enum IntelError {
    #[error("path escapes the indexed workspace: {0}")]
    PathEscape(PathBuf),
    #[error("unsupported source language for {0}")]
    UnsupportedLanguage(PathBuf),
    #[error("source file exceeds the {limit}-byte index limit: {path}")]
    FileTooLarge { path: PathBuf, limit: usize },
    #[error("could not read indexed source {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("tree-sitter rejected the {0:?} grammar")]
    Grammar(Language),
    #[error("tree-sitter could not parse {0}")]
    Parse(PathBuf),
    #[error("the symbol index lock was poisoned")]
    LockPoisoned,
    #[error("the shared symbol index byte budget is exhausted")]
    Capacity,
}

/// Shared admission for retained symbol data across workspace roots and sessions.
#[derive(Debug)]
pub struct IndexBudget {
    maximum: usize,
    retained: AtomicUsize,
    parsing: Mutex<()>,
}

impl IndexBudget {
    #[must_use]
    pub const fn new(maximum: usize) -> Self {
        Self {
            maximum,
            retained: AtomicUsize::new(0),
            parsing: Mutex::new(()),
        }
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.retained.load(Ordering::Acquire)
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<IndexCharge> {
        self.retained
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.maximum)
            })
            .ok()?;
        Some(IndexCharge {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

struct IndexCharge {
    budget: Arc<IndexBudget>,
    bytes: usize,
}

impl Drop for IndexCharge {
    fn drop(&mut self) {
        self.budget.retained.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    length: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileStamp {
    fn new(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        }
    }
}

struct IndexedFile {
    digest: blake3::Hash,
    stamp: Option<FileStamp>,
    seen_in_scan: u64,
    symbols: Vec<Symbol>,
    revision: u64,
    _charge: IndexCharge,
}

struct RankedSymbol<'a> {
    rank: u8,
    symbol: &'a Symbol,
}

impl RankedSymbol<'_> {
    fn key(&self) -> (u8, &str, &Path, usize, usize) {
        (
            self.rank,
            &self.symbol.name,
            &self.symbol.location.path,
            self.symbol.location.line,
            self.symbol.location.column,
        )
    }
}
impl PartialEq for RankedSymbol<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for RankedSymbol<'_> {}
impl PartialOrd for RankedSymbol<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RankedSymbol<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

/// A bounded workspace index. Native parse trees are temporary, never retained.
pub struct SymbolIndex {
    root: PathBuf,
    limits: IndexLimits,
    budget: Arc<IndexBudget>,
    files: RwLock<BTreeMap<PathBuf, IndexedFile>>,
    mutation: Mutex<()>,
    revision: AtomicU64,
    reconciled: Mutex<Option<std::time::Instant>>,
    scan_epoch: AtomicU64,
    scanning: Mutex<()>,
    partial: AtomicBool,
}

impl SymbolIndex {
    /// Create an empty index. The root is canonicalized once to make path checks cheap.
    ///
    /// # Errors
    ///
    /// Returns [`IntelError::Io`] when the workspace root cannot be canonicalized.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, IntelError> {
        let supplied = root.as_ref();
        let root = fs::canonicalize(supplied).map_err(|source| IntelError::Io {
            path: supplied.to_path_buf(),
            source,
        })?;
        Ok(Self {
            root,
            limits: IndexLimits::default(),
            files: RwLock::new(BTreeMap::new()),
            budget: Arc::new(IndexBudget::new(IndexLimits::default().max_retained_bytes)),
            mutation: Mutex::new(()),
            revision: AtomicU64::new(0),
            reconciled: Mutex::new(None),
            scan_epoch: AtomicU64::new(0),
            scanning: Mutex::new(()),
            partial: AtomicBool::new(false),
        })
    }

    /// Override startup and per-file resource bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: IndexLimits) -> Self {
        self.budget = Arc::new(IndexBudget::new(limits.max_retained_bytes));
        self.limits = limits;
        self
    }

    /// Use a runtime-owned budget shared by all admitted workspace indexes.
    #[must_use]
    pub fn with_budget(mut self, budget: Arc<IndexBudget>) -> Self {
        self.budget = budget;
        self
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Whether resource limits omitted or evicted indexed content.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.partial.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reconcile additions, deletions and edits at most once every two seconds.
    /// All users of this root share the same readiness and reconciliation owner.
    ///
    /// # Errors
    /// Returns filesystem, parse or lock failures without advancing freshness.
    pub fn ensure_current(&self) -> Result<(), IntelError> {
        let mut reconciled = self
            .reconciled
            .lock()
            .map_err(|_| IntelError::LockPoisoned)?;
        if reconciled.is_some_and(|last| last.elapsed() < std::time::Duration::from_secs(2)) {
            return Ok(());
        }
        self.index_workspace()?;
        *reconciled = Some(std::time::Instant::now());
        Ok(())
    }

    /// Reconcile recognized, non-ignored files against metadata and content identity.
    ///
    /// # Errors
    ///
    /// Returns an [`IntelError`] if a selected file cannot be read or parsed.
    pub fn index_workspace(&self) -> Result<IndexStats, IntelError> {
        let _scan = self.scanning.lock().map_err(|_| IntelError::LockPoisoned)?;
        let mut stats = IndexStats::default();
        let epoch = self
            .scan_epoch
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let before_revision = self.generation();
        self.partial.store(false, Ordering::Release);
        for (visited, entry) in WalkBuilder::new(&self.root)
            .standard_filters(true)
            .follow_links(false)
            .max_depth(Some(64))
            .build()
            .enumerate()
        {
            if visited >= self.limits.max_scan_entries {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
                break;
            }
            let Ok(entry) = entry else {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
                continue;
            };
            if entry.depth() == 64 && entry.file_type().is_some_and(|kind| kind.is_dir()) {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
            }
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if Language::for_path(entry.path()).is_none() {
                continue;
            }
            if stats.indexed_files >= self.limits.max_files {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
                break;
            }
            let Ok(metadata) = entry.metadata() else {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
                continue;
            };
            if metadata.len() > self.limits.max_file_bytes as u64 {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
                continue;
            }
            match self.update_path(entry.path()) {
                Ok(_) => {}
                Err(IntelError::Capacity) => {
                    stats.skipped_files = stats.skipped_files.saturating_add(1);
                    continue;
                }
                Err(error) => return Err(error),
            }
            if let Ok(relative) = entry.path().strip_prefix(&self.root)
                && let Some(file) = self
                    .files
                    .write()
                    .map_err(|_| IntelError::LockPoisoned)?
                    .get_mut(relative)
            {
                file.seen_in_scan = epoch;
            }
            stats.indexed_files = stats.indexed_files.saturating_add(1);
        }
        let mut files = self.files.write().map_err(|_| IntelError::LockPoisoned)?;
        let before = files.len();
        files.retain(|_, file| file.seen_in_scan == epoch || file.revision > before_revision);
        if before != files.len() {
            self.revision.fetch_add(1, Ordering::AcqRel);
        }
        stats.indexed_files = files.len();
        if stats.skipped_files > 0 {
            self.partial.store(true, Ordering::Release);
        }
        stats.symbols = files.values().map(|file| file.symbols.len()).sum();
        Ok(stats)
    }

    /// Refresh one file, skipping an unchanged descriptor identity.
    ///
    /// # Errors
    ///
    /// Returns an [`IntelError`] for escaped paths, unsupported/oversized sources, I/O failures,
    /// or parser failures.
    pub fn update_path(&self, path: impl AsRef<Path>) -> Result<usize, IntelError> {
        let _parsing = self
            .budget
            .parsing
            .lock()
            .map_err(|_| IntelError::LockPoisoned)?;
        let relative = self.relative_path(path.as_ref())?;
        let absolute = self.root.join(&relative);
        let file = fs::File::open(&absolute).map_err(|source| IntelError::Io {
            path: relative.clone(),
            source,
        })?;
        let stamp = file
            .metadata()
            .map(|metadata| FileStamp::new(&metadata))
            .map_err(|source| IntelError::Io {
                path: relative.clone(),
                source,
            })?;
        if let Some(cached) = self
            .files
            .read()
            .map_err(|_| IntelError::LockPoisoned)?
            .get(&relative)
            && stamp.modified.is_some()
            && cached.stamp.as_ref() == Some(&stamp)
        {
            return Ok(cached.symbols.len());
        }
        let mut source = String::new();
        (&file)
            .take(self.limits.max_file_bytes.saturating_add(1) as u64)
            .read_to_string(&mut source)
            .map_err(|source| IntelError::Io {
                path: relative.clone(),
                source,
            })?;
        if source.len() > self.limits.max_file_bytes {
            self.remove_path(&relative)?;
            self.partial.store(true, Ordering::Release);
            return Err(IntelError::FileTooLarge {
                path: relative,
                limit: self.limits.max_file_bytes,
            });
        }
        let after = file
            .metadata()
            .map(|metadata| FileStamp::new(&metadata))
            .map_err(|source| IntelError::Io {
                path: relative.clone(),
                source,
            })?;
        if after != stamp {
            self.remove_path(&relative)?;
            return Err(IntelError::Io {
                path: relative,
                source: std::io::Error::other("source changed during index read"),
            });
        }
        let count = self.update_content(&relative, &source)?;
        let mut files = self.files.write().map_err(|_| IntelError::LockPoisoned)?;
        if let Some(cached) = files.get_mut(&relative)
            && cached.digest == blake3::hash(source.as_bytes())
        {
            cached.stamp = Some(stamp);
        }
        Ok(count)
    }

    /// Replace symbols for changed caller-supplied source without touching disk.
    ///
    /// # Errors
    ///
    /// Returns an [`IntelError`] for escaped paths, unsupported/oversized sources, or parser
    /// failures.
    pub fn update_source(&self, path: impl AsRef<Path>, source: &str) -> Result<usize, IntelError> {
        let _parsing = self
            .budget
            .parsing
            .lock()
            .map_err(|_| IntelError::LockPoisoned)?;
        self.update_content(path.as_ref(), source)
    }

    fn update_content(&self, path: &Path, source: &str) -> Result<usize, IntelError> {
        let relative = normalize_relative(path)?;
        if source.len() > self.limits.max_file_bytes {
            self.remove_path(&relative)?;
            self.partial.store(true, Ordering::Release);
            return Err(IntelError::FileTooLarge {
                path: relative,
                limit: self.limits.max_file_bytes,
            });
        }
        let language = Language::for_path(&relative)
            .ok_or_else(|| IntelError::UnsupportedLanguage(relative.clone()))?;
        let _mutation = self.mutation.lock().map_err(|_| IntelError::LockPoisoned)?;
        let digest = blake3::hash(source.as_bytes());
        if let Some(old) = self
            .files
            .read()
            .map_err(|_| IntelError::LockPoisoned)?
            .get(&relative)
            && old.digest == digest
        {
            return Ok(old.symbols.len());
        }
        let mut parser = Parser::new();
        parser
            .set_language(&language.parser_language(&relative))
            .map_err(|_| IntelError::Grammar(language))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| IntelError::Parse(relative.clone()))?;
        let symbol_limit = self.limits.max_symbols_per_file.min(
            (8 * 1024 * 1024) / (std::mem::size_of::<Symbol>() + relative.as_os_str().len() + 128),
        );
        let symbols = extract_symbols(&relative, source, language, &tree, symbol_limit);
        let count = symbols.len();
        if count >= symbol_limit {
            self.partial.store(true, Ordering::Release);
        }
        let bytes = symbols
            .capacity()
            .saturating_mul(std::mem::size_of::<Symbol>())
            .saturating_add(
                symbols
                    .iter()
                    .map(|symbol| {
                        symbol
                            .name
                            .capacity()
                            .saturating_add(symbol.location.path.capacity())
                    })
                    .fold(0_usize, usize::saturating_add),
            )
            .saturating_add(relative.capacity())
            .saturating_add(std::mem::size_of::<IndexedFile>() + 128);
        let mut files = self.files.write().map_err(|_| IntelError::LockPoisoned)?;
        if files.remove(&relative).is_some() {
            self.revision.fetch_add(1, Ordering::AcqRel);
        }
        if bytes > self.budget.maximum {
            self.partial.store(true, Ordering::Release);
            return Err(IntelError::Capacity);
        }
        let charge = loop {
            if files.len() < self.limits.max_files
                && let Some(charge) = self.budget.reserve(bytes)
            {
                break charge;
            }
            let oldest = files
                .iter()
                .min_by_key(|(_, file)| file.revision)
                .map(|(path, _)| path.clone());
            let Some(oldest) = oldest else {
                self.partial.store(true, Ordering::Release);
                return Err(IntelError::Capacity);
            };
            files.remove(&oldest);
            self.partial.store(true, Ordering::Release);
            self.revision.fetch_add(1, Ordering::AcqRel);
        };
        let revision = self
            .revision
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        files.insert(
            relative,
            IndexedFile {
                digest,
                stamp: None,
                seen_in_scan: 0,
                symbols,
                revision,
                _charge: charge,
            },
        );
        Ok(count)
    }

    /// Remove a file after a watcher reports deletion.
    ///
    /// # Errors
    ///
    /// Returns [`IntelError::PathEscape`] for non-workspace-relative paths or
    /// [`IntelError::LockPoisoned`] when index state is unavailable.
    pub fn remove_path(&self, path: impl AsRef<Path>) -> Result<bool, IntelError> {
        let relative = normalize_relative(path.as_ref())?;
        let removed = self
            .files
            .write()
            .map_err(|_| IntelError::LockPoisoned)?
            .remove(&relative)
            .is_some();
        if removed {
            self.revision.fetch_add(1, Ordering::AcqRel);
        }
        Ok(removed)
    }

    /// Search definitions and references, ranking exact names before prefix and substring hits.
    ///
    /// # Errors
    ///
    /// Returns [`IntelError::LockPoisoned`] when index state is unavailable.
    pub fn query(&self, query: &SymbolQuery) -> Result<Vec<Symbol>, IntelError> {
        let limit = query.limit.clamp(1, 10_000);
        let needle = query.pattern.to_lowercase();
        let files = self.files.read().map_err(|_| IntelError::LockPoisoned)?;
        let mut matches = BinaryHeap::with_capacity(limit);
        for symbol in files.values().flat_map(|file| &file.symbols) {
            if (!query.roles.is_empty() && !query.roles.contains(&symbol.role))
                || (!query.languages.is_empty() && !query.languages.contains(&symbol.language))
            {
                continue;
            }
            let name = symbol.name.to_lowercase();
            let rank = if name == needle {
                0
            } else if name.starts_with(&needle) {
                1
            } else if name.contains(&needle) {
                2
            } else {
                continue;
            };
            let hit = RankedSymbol { rank, symbol };
            if matches.len() < limit {
                matches.push(hit);
            } else if matches.peek().is_some_and(|worst| &hit < worst) {
                matches.pop();
                matches.push(hit);
            }
        }
        Ok(matches
            .into_sorted_vec()
            .into_iter()
            .map(|hit| hit.symbol.clone())
            .collect())
    }

    /// Return all indexed occurrences for a single file.
    ///
    /// # Errors
    ///
    /// Returns [`IntelError::PathEscape`] for non-workspace-relative paths or
    /// [`IntelError::LockPoisoned`] when index state is unavailable.
    pub fn symbols_for_file(&self, path: impl AsRef<Path>) -> Result<Vec<Symbol>, IntelError> {
        let relative = normalize_relative(path.as_ref())?;
        Ok(self
            .files
            .read()
            .map_err(|_| IntelError::LockPoisoned)?
            .get(&relative)
            .map_or_else(Vec::new, |file| file.symbols.clone()))
    }

    fn relative_path(&self, path: &Path) -> Result<PathBuf, IntelError> {
        let absolute = if path.is_absolute() {
            fs::canonicalize(path).map_err(|source| IntelError::Io {
                path: path.to_path_buf(),
                source,
            })?
        } else {
            fs::canonicalize(self.root.join(path)).map_err(|source| IntelError::Io {
                path: path.to_path_buf(),
                source,
            })?
        };
        absolute
            .strip_prefix(&self.root)
            .map_err(|_| IntelError::PathEscape(path.to_path_buf()))
            .and_then(normalize_relative)
    }
}

fn normalize_relative(path: &Path) -> Result<PathBuf, IntelError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(IntelError::PathEscape(path.to_path_buf()));
            }
        }
    }
    Ok(normalized)
}

fn extract_symbols(
    path: &Path,
    source: &str,
    language: Language,
    tree: &Tree,
    limit: usize,
) -> Vec<Symbol> {
    let mut definitions = Vec::new();
    let mut definition_ranges = HashSet::new();
    visit(tree.root_node(), &mut |node| {
        if definitions.len() >= limit {
            return;
        }
        let Some(kind) = definition_kind(language, node.kind()) else {
            return;
        };
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        if let Ok(name) = name_node.utf8_text(source.as_bytes())
            && !name.is_empty()
        {
            definition_ranges.insert((name_node.start_byte(), name_node.end_byte()));
            definitions.push(symbol_from_node(
                path,
                name,
                kind,
                SymbolRole::Definition,
                language,
                name_node,
            ));
        }
    });

    if definitions.len() >= limit {
        definitions.truncate(limit);
        return definitions;
    }
    let mut symbols = definitions;
    visit(tree.root_node(), &mut |node| {
        if symbols.len() >= limit
            || !is_identifier(language, node.kind())
            || definition_ranges.contains(&(node.start_byte(), node.end_byte()))
        {
            return;
        }
        if let Ok(name) = node.utf8_text(source.as_bytes())
            && !name.is_empty()
        {
            symbols.push(symbol_from_node(
                path,
                name,
                SymbolKind::Identifier,
                SymbolRole::Reference,
                language,
                node,
            ));
        }
    });
    symbols
}

fn visit(node: Node<'_>, callback: &mut impl FnMut(Node<'_>)) {
    let mut cursor = node.walk();
    loop {
        callback(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn symbol_from_node(
    path: &Path,
    name: &str,
    kind: SymbolKind,
    role: SymbolRole,
    language: Language,
    node: Node<'_>,
) -> Symbol {
    let start = node.start_position();
    let end = node.end_position();
    Symbol {
        name: name.to_owned(),
        kind,
        role,
        language,
        location: SourceLocation {
            path: path.to_path_buf(),
            line: start.row + 1,
            column: start.column + 1,
            end_line: end.row + 1,
            end_column: end.column + 1,
        },
    }
}

fn definition_kind(language: Language, node_kind: &str) -> Option<SymbolKind> {
    match (language, node_kind) {
        (Language::Rust, "function_item")
        | (Language::Python, "function_definition")
        | (Language::TypeScript, "function_declaration") => Some(SymbolKind::Function),
        (Language::TypeScript, "method_definition") => Some(SymbolKind::Method),
        (Language::Python | Language::TypeScript, "class_definition" | "class_declaration") => {
            Some(SymbolKind::Class)
        }
        (Language::Rust, "struct_item") => Some(SymbolKind::Struct),
        (Language::Rust | Language::TypeScript, "enum_item" | "enum_declaration") => {
            Some(SymbolKind::Enum)
        }
        (Language::TypeScript, "interface_declaration") => Some(SymbolKind::Interface),
        (Language::Rust, "trait_item") => Some(SymbolKind::Trait),
        (Language::Rust, "mod_item") => Some(SymbolKind::Module),
        (Language::Rust | Language::TypeScript, "type_item" | "type_alias_declaration") => {
            Some(SymbolKind::Type)
        }
        (Language::Rust, "const_item" | "static_item") => Some(SymbolKind::Constant),
        _ => None,
    }
}

fn is_identifier(language: Language, node_kind: &str) -> bool {
    match language {
        Language::Rust => matches!(node_kind, "identifier" | "type_identifier"),
        Language::Python => node_kind == "identifier",
        Language::TypeScript => matches!(
            node_kind,
            "identifier" | "property_identifier" | "type_identifier"
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn scan_entry_and_symbol_limits_report_partial_results() {
        let root = tempdir().expect("root");
        fs::write(root.path().join("one.rs"), "struct First; struct Second;").expect("write");
        let index = SymbolIndex::new(root.path())
            .expect("index")
            .with_limits(IndexLimits {
                max_scan_entries: 1,
                ..IndexLimits::default()
            });
        assert_eq!(index.index_workspace().expect("scan").indexed_files, 0);
        assert!(index.is_partial());
        let index = SymbolIndex::new(root.path())
            .expect("index")
            .with_limits(IndexLimits {
                max_symbols_per_file: 1,
                ..IndexLimits::default()
            });
        index.index_workspace().expect("scan");
        assert_eq!(index.symbols_for_file("one.rs").expect("query").len(), 1);
        assert!(index.is_partial());
    }

    #[test]
    fn oversized_replacement_invalidates_cached_symbols() {
        let root = tempdir().expect("root");
        let index = SymbolIndex::new(root.path())
            .expect("index")
            .with_limits(IndexLimits {
                max_file_bytes: 20,
                ..IndexLimits::default()
            });
        index
            .update_source("one.rs", "struct First;")
            .expect("source");
        assert!(index.update_source("one.rs", &" ".repeat(21)).is_err());
        assert!(index.symbols_for_file("one.rs").expect("query").is_empty());
        assert!(index.is_partial());
    }

    #[test]
    fn shared_budget_bounds_roots_and_refunds_eviction_and_drop() {
        let first = tempdir().expect("root");
        let second = tempdir().expect("root");
        let budget = Arc::new(IndexBudget::new(4096));
        let left = SymbolIndex::new(first.path())
            .expect("index")
            .with_budget(Arc::clone(&budget));
        let right = SymbolIndex::new(second.path())
            .expect("index")
            .with_budget(Arc::clone(&budget));
        left.update_source("one.rs", "struct Alpha;")
            .expect("source");
        right
            .update_source("two.rs", "struct Beta;")
            .expect("source");
        for n in 0..100 {
            let _ = left.update_source(format!("file{n}.rs"), &format!("struct Item{n};"));
            assert!(budget.retained_bytes() <= 4096);
        }
        assert!(left.is_partial());
        assert!(!right.symbols_for_file("two.rs").expect("query").is_empty());
        drop(left);
        assert!(budget.retained_bytes() > 0);
        drop(right);
        assert_eq!(budget.retained_bytes(), 0);
    }

    #[test]
    fn reconciliation_updates_external_edits_additions_and_deletions() {
        let root = tempdir().expect("root");
        fs::write(root.path().join("one.rs"), "struct Alpha;").expect("write");
        let index = SymbolIndex::new(root.path()).expect("index");
        index.ensure_current().expect("initial");
        let initial = index.generation();
        index.index_workspace().expect("unchanged");
        assert_eq!(index.generation(), initial);
        fs::write(root.path().join("one.rs"), "struct Bravo;").expect("external edit");
        fs::write(root.path().join("two.rs"), "struct Charlie;").expect("external add");
        index.index_workspace().expect("reconcile");
        assert!(index.generation() > initial);
        assert_eq!(
            index.symbols_for_file("one.rs").expect("symbols")[0].name,
            "Bravo"
        );
        fs::remove_file(root.path().join("one.rs")).expect("delete");
        index.index_workspace().expect("reconcile");
        assert!(
            index
                .symbols_for_file("one.rs")
                .expect("symbols")
                .is_empty()
        );
        assert!(
            !index
                .symbols_for_file("two.rs")
                .expect("symbols")
                .is_empty()
        );
    }

    #[test]
    fn bounded_query_keeps_exact_rank_order() {
        let root = tempdir().expect("root");
        let index = SymbolIndex::new(root.path()).expect("index");
        for name in ["Dogfish", "WildDog", "Dog", "Doghouse", "Underdog"] {
            index
                .update_source(format!("{name}.rs"), &format!("struct {name};"))
                .expect("source");
        }
        let result = index
            .query(&SymbolQuery {
                pattern: "dog".to_owned(),
                roles: vec![SymbolRole::Definition],
                languages: vec![],
                limit: 2,
            })
            .expect("query");
        assert_eq!(
            result
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            ["Dog", "Dogfish"]
        );
    }

    #[test]
    fn indexes_and_incrementally_updates_three_languages() {
        let root = tempdir().expect("temp directory");
        fs::write(
            root.path().join("lib.rs"),
            "pub struct HoundDog;\nfn rust_run() { let _ = HoundDog; }\n",
        )
        .expect("rust fixture");
        fs::write(
            root.path().join("app.py"),
            "class Kennel:\n    pass\n\ndef python_run():\n    return Kennel()\n",
        )
        .expect("python fixture");
        fs::write(
            root.path().join("index.ts"),
            "interface Collar { tag: string }\nfunction tsRun(): Collar { return { tag: 'r' }; }\n",
        )
        .expect("typescript fixture");

        let index = SymbolIndex::new(root.path()).expect("index");
        let stats = index.index_workspace().expect("workspace indexing");
        assert_eq!(stats.indexed_files, 3);
        for (name, language) in [
            ("HoundDog", Language::Rust),
            ("Kennel", Language::Python),
            ("Collar", Language::TypeScript),
        ] {
            let results = index
                .query(&SymbolQuery {
                    pattern: name.to_owned(),
                    roles: vec![SymbolRole::Definition],
                    languages: vec![language],
                    limit: 10,
                })
                .expect("query");
            assert_eq!(results.len(), 1, "missing {name:?}");
        }

        index
            .update_source("lib.rs", "pub struct Greyhound;\nfn rust_run() {}\n")
            .expect("incremental update");
        let removed = index
            .query(&SymbolQuery {
                pattern: "HoundDog".to_owned(),
                roles: vec![SymbolRole::Definition],
                languages: vec![],
                limit: 10,
            })
            .expect("old query");
        assert!(removed.is_empty());
        let added = index
            .query(&SymbolQuery {
                pattern: "Greyhound".to_owned(),
                roles: vec![SymbolRole::Definition],
                languages: vec![],
                limit: 10,
            })
            .expect("new query");
        assert_eq!(added.len(), 1);
    }

    #[test]
    fn rejects_parent_traversal_and_caps_large_sources() {
        let root = tempdir().expect("temp directory");
        let index = SymbolIndex::new(root.path())
            .expect("index")
            .with_limits(IndexLimits {
                max_file_bytes: 8,
                ..IndexLimits::default()
            });
        assert!(matches!(
            index.update_source("../escape.rs", "fn x() {}"),
            Err(IntelError::PathEscape(_))
        ));
        assert!(matches!(
            index.update_source("large.rs", "fn larger() {}"),
            Err(IntelError::FileTooLarge { .. })
        ));
    }

    #[test]
    fn references_are_indexed_separately_from_definitions() {
        let root = tempdir().expect("temp directory");
        let index = SymbolIndex::new(root.path()).expect("index");
        index
            .update_source("lib.rs", "struct Dog; fn use_it() { let _ = Dog; }")
            .expect("source update");
        let references = index
            .query(&SymbolQuery {
                pattern: "Dog".to_owned(),
                roles: vec![SymbolRole::Reference],
                languages: vec![],
                limit: 10,
            })
            .expect("reference query");
        assert_eq!(references.len(), 1);
    }
}
