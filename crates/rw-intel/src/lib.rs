//! Incremental, local code intelligence backed by tree-sitter.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

/// Languages supported by the always-on syntax index.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Python,
    TypeScript,
}

impl Language {
    fn for_path(path: &Path) -> Option<Self> {
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
    pub max_file_bytes: usize,
    pub max_symbols_per_file: usize,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_file_bytes: 2 * 1024 * 1024,
            max_symbols_per_file: 50_000,
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
}

struct IndexedFile {
    source: String,
    tree: Tree,
    symbols: Vec<Symbol>,
}

/// A workspace-relative tree-sitter index with per-file incremental reparsing.
pub struct SymbolIndex {
    root: PathBuf,
    limits: IndexLimits,
    files: RwLock<BTreeMap<PathBuf, IndexedFile>>,
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
        })
    }

    /// Override startup and per-file resource bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: IndexLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Index recognized, non-ignored files. Existing entries are incrementally updated.
    ///
    /// # Errors
    ///
    /// Returns an [`IntelError`] if a selected file cannot be read or parsed.
    pub fn index_workspace(&self) -> Result<IndexStats, IntelError> {
        let mut stats = IndexStats::default();
        let mut retained_paths = HashSet::new();
        for entry in WalkBuilder::new(&self.root)
            .standard_filters(true)
            .follow_links(false)
            .sort_by_file_path(Path::cmp)
            .build()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if Language::for_path(entry.path()).is_none() {
                continue;
            }
            if stats.indexed_files >= self.limits.max_files {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
                continue;
            };
            if metadata.len() > self.limits.max_file_bytes as u64 {
                stats.skipped_files = stats.skipped_files.saturating_add(1);
                continue;
            }
            self.update_path(entry.path())?;
            if let Ok(relative) = entry.path().strip_prefix(&self.root) {
                retained_paths.insert(relative.to_path_buf());
            }
            stats.indexed_files = stats.indexed_files.saturating_add(1);
        }
        let mut files = self.files.write().map_err(|_| IntelError::LockPoisoned)?;
        files.retain(|path, _| retained_paths.contains(path));
        stats.symbols = files.values().map(|file| file.symbols.len()).sum();
        Ok(stats)
    }

    /// Read and incrementally update one source file.
    ///
    /// # Errors
    ///
    /// Returns an [`IntelError`] for escaped paths, unsupported/oversized sources, I/O failures,
    /// or parser failures.
    pub fn update_path(&self, path: impl AsRef<Path>) -> Result<usize, IntelError> {
        let relative = self.relative_path(path.as_ref())?;
        let absolute = self.root.join(&relative);
        let file = fs::File::open(&absolute).map_err(|source| IntelError::Io {
            path: relative.clone(),
            source,
        })?;
        let mut source = String::new();
        file.take(self.limits.max_file_bytes.saturating_add(1) as u64)
            .read_to_string(&mut source)
            .map_err(|source| IntelError::Io {
                path: relative.clone(),
                source,
            })?;
        if source.len() > self.limits.max_file_bytes {
            return Err(IntelError::FileTooLarge {
                path: relative,
                limit: self.limits.max_file_bytes,
            });
        }
        self.update_source(relative, &source)
    }

    /// Incrementally update a file from caller-supplied source without touching disk.
    ///
    /// # Errors
    ///
    /// Returns an [`IntelError`] for escaped paths, unsupported/oversized sources, or parser
    /// failures.
    pub fn update_source(&self, path: impl AsRef<Path>, source: &str) -> Result<usize, IntelError> {
        let relative = normalize_relative(path.as_ref())?;
        if source.len() > self.limits.max_file_bytes {
            return Err(IntelError::FileTooLarge {
                path: relative,
                limit: self.limits.max_file_bytes,
            });
        }
        let language = Language::for_path(&relative)
            .ok_or_else(|| IntelError::UnsupportedLanguage(relative.clone()))?;
        let old_file = self
            .files
            .read()
            .map_err(|_| IntelError::LockPoisoned)?
            .get(&relative)
            .map(|old| (old.source.clone(), old.tree.clone(), old.symbols.len()));
        if let Some((old_source, _, count)) = &old_file
            && old_source == source
        {
            return Ok(*count);
        }
        let mut parser = Parser::new();
        parser
            .set_language(&language.parser_language(&relative))
            .map_err(|_| IntelError::Grammar(language))?;

        let old_tree = old_file.map(|(old_source, mut tree, _)| {
            tree.edit(&single_edit(&old_source, source));
            tree
        });
        let tree = parser
            .parse(source, old_tree.as_ref())
            .ok_or_else(|| IntelError::Parse(relative.clone()))?;
        let symbols = extract_symbols(
            &relative,
            source,
            language,
            &tree,
            self.limits.max_symbols_per_file,
        );
        let count = symbols.len();
        self.files
            .write()
            .map_err(|_| IntelError::LockPoisoned)?
            .insert(
                relative,
                IndexedFile {
                    source: source.to_owned(),
                    tree,
                    symbols,
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
        Ok(self
            .files
            .write()
            .map_err(|_| IntelError::LockPoisoned)?
            .remove(&relative)
            .is_some())
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
        let mut matches: Vec<(u8, Symbol)> = files
            .values()
            .flat_map(|file| file.symbols.iter())
            .filter(|symbol| query.roles.is_empty() || query.roles.contains(&symbol.role))
            .filter(|symbol| {
                query.languages.is_empty() || query.languages.contains(&symbol.language)
            })
            .filter_map(|symbol| {
                let name = symbol.name.to_lowercase();
                let rank = if name == needle {
                    0
                } else if name.starts_with(&needle) {
                    1
                } else if name.contains(&needle) {
                    2
                } else {
                    return None;
                };
                Some((rank, symbol.clone()))
            })
            .collect();
        matches.sort_by(|(left_rank, left), (right_rank, right)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.location.path.cmp(&right.location.path))
                .then_with(|| left.location.line.cmp(&right.location.line))
                .then_with(|| left.location.column.cmp(&right.location.column))
        });
        matches.truncate(limit);
        Ok(matches.into_iter().map(|(_, symbol)| symbol).collect())
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

fn single_edit(old: &str, new: &str) -> InputEdit {
    let old_bytes = old.as_bytes();
    let new_bytes = new.as_bytes();
    let mut prefix = 0;
    while prefix < old_bytes.len()
        && prefix < new_bytes.len()
        && old_bytes[prefix] == new_bytes[prefix]
    {
        prefix += 1;
    }
    while prefix > 0 && (!old.is_char_boundary(prefix) || !new.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    let mut old_suffix = old_bytes.len();
    let mut new_suffix = new_bytes.len();
    while old_suffix > prefix
        && new_suffix > prefix
        && old_bytes[old_suffix - 1] == new_bytes[new_suffix - 1]
    {
        old_suffix -= 1;
        new_suffix -= 1;
    }
    while old_suffix < old.len() && !old.is_char_boundary(old_suffix) {
        old_suffix += 1;
    }
    while new_suffix < new.len() && !new.is_char_boundary(new_suffix) {
        new_suffix += 1;
    }

    InputEdit {
        start_byte: prefix,
        old_end_byte: old_suffix,
        new_end_byte: new_suffix,
        start_position: point_for_byte(old, prefix),
        old_end_position: point_for_byte(old, old_suffix),
        new_end_position: point_for_byte(new, new_suffix),
    }
}

fn point_for_byte(source: &str, byte: usize) -> Point {
    let prefix = &source[..byte];
    let row = prefix.bytes().filter(|value| *value == b'\n').count();
    let column = prefix
        .rfind('\n')
        .map_or(prefix.len(), |position| prefix.len() - position - 1);
    Point::new(row, column)
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
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        callback(current);
        let mut cursor = current.walk();
        let children: Vec<_> = current.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
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
