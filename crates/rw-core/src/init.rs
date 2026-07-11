//! Deterministic repository analysis for `/init` and `/deep-init`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

/// Default maximum size of each generated human-owned instruction file.
pub const DEFAULT_INIT_FILE_BUDGET_BYTES: usize = 16 * 1024;
/// Maximum filesystem entries inspected by one initialization analysis.
pub const MAX_INIT_SCAN_ENTRIES: usize = 50_000;
const MAX_MARKER_BYTES: u64 = 256 * 1024;
const MAX_DEEP_FILES: usize = 64;

/// Requested initialization depth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitDepth {
    /// Generate only root `AGENTS.md`.
    Root,
    /// Generate root plus major subsystem `AGENTS.md` files.
    Deep,
}

/// A deterministic, reviewable set of files to create.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitPlan {
    root: PathBuf,
    files: BTreeMap<PathBuf, String>,
    skipped_directories: Vec<PathBuf>,
}

impl InitPlan {
    /// Canonical workspace root analyzed by this plan.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Workspace-relative generated files and exact contents.
    #[must_use]
    pub fn files(&self) -> &BTreeMap<PathBuf, String> {
        &self.files
    }

    /// Vendored/generated directories deliberately excluded from analysis.
    #[must_use]
    pub fn skipped_directories(&self) -> &[PathBuf] {
        &self.skipped_directories
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Ecosystem {
    Rust,
    TypeScript,
    Python,
}

impl Ecosystem {
    const fn label(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::TypeScript => "TypeScript/JavaScript",
            Self::Python => "Python",
        }
    }
}

#[derive(Debug)]
struct Analysis {
    ecosystems: BTreeSet<Ecosystem>,
    test_commands: BTreeSet<String>,
    check_commands: BTreeSet<String>,
    subsystems: BTreeSet<PathBuf>,
    skipped: BTreeSet<PathBuf>,
    entries: usize,
}

impl Analysis {
    fn new() -> Self {
        Self {
            ecosystems: BTreeSet::new(),
            test_commands: BTreeSet::new(),
            check_commands: BTreeSet::new(),
            subsystems: BTreeSet::new(),
            skipped: BTreeSet::new(),
            entries: 0,
        }
    }
}

/// Analyzes a repository without executing project code and builds generated
/// `AGENTS.md` contents.
///
/// The traversal is deterministic, does not follow symlinks, and skips common
/// vendored/generated directories. Marker reads and traversal counts are
/// bounded. The returned plan performs no writes until [`apply_init_plan`].
///
/// # Errors
///
/// Returns an error for an invalid workspace, unsafe marker, excessive tree,
/// malformed package metadata, or a generated file exceeding `file_budget`.
pub fn plan_init(
    workspace_root: &Path,
    depth: InitDepth,
    file_budget: usize,
) -> Result<InitPlan, InitError> {
    if file_budget == 0 {
        return Err(InitError::InvalidBudget);
    }
    let root = fs::canonicalize(workspace_root).map_err(|source| InitError::Io {
        operation: "canonicalize workspace",
        path: workspace_root.to_path_buf(),
        source,
    })?;
    if !root.is_dir() {
        return Err(InitError::WorkspaceNotDirectory);
    }
    let mut analysis = Analysis::new();
    scan_directory(&root, &root, &mut analysis)?;
    detect_root_commands(&root, &mut analysis)?;

    let deep_paths = if depth == InitDepth::Deep {
        analysis
            .subsystems
            .iter()
            .filter(|path| !path.as_os_str().is_empty())
            .take(MAX_DEEP_FILES)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if depth == InitDepth::Deep && analysis.subsystems.len() > MAX_DEEP_FILES {
        return Err(InitError::TooManySubsystems {
            count: analysis.subsystems.len(),
            limit: MAX_DEEP_FILES,
        });
    }

    let mut files = BTreeMap::new();
    let root_content = render_root(&analysis, &deep_paths);
    ensure_budget(Path::new("AGENTS.md"), &root_content, file_budget)?;
    files.insert(PathBuf::from("AGENTS.md"), root_content);
    for path in deep_paths {
        let content = render_subsystem(&root, &path);
        let target = path.join("AGENTS.md");
        ensure_budget(&target, &content, file_budget)?;
        files.insert(target, content);
    }
    Ok(InitPlan {
        root,
        files,
        skipped_directories: analysis.skipped.into_iter().collect(),
    })
}

/// Creates every file in an initialization plan without overwriting existing
/// human-owned instructions.
///
/// # Errors
///
/// Fails before writing when any destination already exists or has an unsafe
/// type. On Unix, creation and rollback are relative to pinned, no-follow
/// directory handles so parent-path swaps cannot redirect writes.
pub fn apply_init_plan(plan: &InitPlan) -> Result<Vec<PathBuf>, InitError> {
    #[cfg(unix)]
    {
        apply_init_plan_unix(plan, || {})
    }
    #[cfg(not(unix))]
    {
        let _ = plan;
        Err(InitError::SecureCreationUnsupported)
    }
}

#[cfg(unix)]
struct PreparedInitTarget {
    relative: PathBuf,
    parent: std::os::fd::OwnedFd,
    name: std::ffi::OsString,
    parent_stat: rustix::fs::Stat,
}

#[cfg(unix)]
fn apply_init_plan_unix(
    plan: &InitPlan,
    after_preflight: impl FnOnce(),
) -> Result<Vec<PathBuf>, InitError> {
    use rustix::fs::{AtFlags, Mode, OFlags};

    let root = rustix::fs::open(
        &plan.root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|source| InitError::Io {
        operation: "pin canonical initialization root",
        path: PathBuf::from("."),
        source: source.into(),
    })?;
    let mut prepared = Vec::with_capacity(plan.files.len());
    for relative in plan.files.keys() {
        let (parent, name) = open_init_parent(&root, relative)?;
        match rustix::fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => {
                return Err(InitError::ExistingInstructions {
                    path: relative.clone(),
                });
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                return Err(InitError::UnsafePath {
                    path: relative.clone(),
                });
            }
            Err(source) => {
                return Err(InitError::Io {
                    operation: "inspect generated instruction destination",
                    path: relative.clone(),
                    source: source.into(),
                });
            }
        }
        let stat = rustix::fs::fstat(&parent).map_err(|source| InitError::Io {
            operation: "inspect pinned instruction parent",
            path: relative.clone(),
            source: source.into(),
        })?;
        prepared.push(PreparedInitTarget {
            relative: relative.clone(),
            parent,
            name,
            parent_stat: stat,
        });
    }

    after_preflight();
    let mut created = Vec::with_capacity(prepared.len());
    let result = (|| {
        for target in &prepared {
            ensure_init_parent_unchanged(&root, target)?;
            let descriptor = rustix::fs::openat(
                &target.parent,
                &target.name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o644),
            )
            .map_err(|source| InitError::Io {
                operation: "create generated instructions",
                path: target.relative.clone(),
                source: source.into(),
            })?;
            created.push(target.relative.clone());
            let mut file = File::from(descriptor);
            file.write_all(plan.files[&target.relative].as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|source| InitError::Io {
                    operation: "persist generated instructions",
                    path: target.relative.clone(),
                    source,
                })?;
            rustix::fs::fsync(&target.parent).map_err(|source| InitError::Io {
                operation: "sync generated instruction parent",
                path: target.relative.clone(),
                source: source.into(),
            })?;
        }
        for target in &prepared {
            ensure_init_parent_unchanged(&root, target)?;
        }
        rustix::fs::fsync(&root).map_err(|source| InitError::Io {
            operation: "sync workspace after initialization",
            path: PathBuf::from("."),
            source: source.into(),
        })?;
        Ok::<(), InitError>(())
    })();
    if let Err(error) = result {
        for target in prepared.iter().rev() {
            if created.contains(&target.relative) {
                let _ = rustix::fs::unlinkat(&target.parent, &target.name, AtFlags::empty());
                let _ = rustix::fs::fsync(&target.parent);
            }
        }
        let _ = rustix::fs::fsync(&root);
        return Err(error);
    }
    Ok(created)
}

#[cfg(unix)]
fn open_init_parent(
    root: &std::os::fd::OwnedFd,
    relative: &Path,
) -> Result<(std::os::fd::OwnedFd, std::ffi::OsString), InitError> {
    use rustix::fs::{Mode, OFlags};

    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(InitError::UnsafePath {
            path: relative.to_path_buf(),
        });
    }
    let mut components = relative.components().collect::<Vec<_>>();
    let Some(Component::Normal(name)) = components.pop() else {
        return Err(InitError::UnsafePath {
            path: relative.to_path_buf(),
        });
    };
    let mut directory = root.try_clone().map_err(|source| InitError::Io {
        operation: "clone pinned initialization root",
        path: relative.to_path_buf(),
        source,
    })?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(InitError::UnsafePath {
                path: relative.to_path_buf(),
            });
        };
        directory = rustix::fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|source| match source {
            rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => InitError::UnsafePath {
                path: relative.to_path_buf(),
            },
            _ => InitError::Io {
                operation: "open generated instruction parent without following links",
                path: relative.to_path_buf(),
                source: source.into(),
            },
        })?;
    }
    Ok((directory, name.to_os_string()))
}

#[cfg(unix)]
fn ensure_init_parent_unchanged(
    root: &std::os::fd::OwnedFd,
    target: &PreparedInitTarget,
) -> Result<(), InitError> {
    let (current, _) = open_init_parent(root, &target.relative)?;
    let stat = rustix::fs::fstat(&current).map_err(|source| InitError::Io {
        operation: "revalidate generated instruction parent",
        path: target.relative.clone(),
        source: source.into(),
    })?;
    if stat.st_dev != target.parent_stat.st_dev || stat.st_ino != target.parent_stat.st_ino {
        return Err(InitError::UnsafePath {
            path: target.relative.clone(),
        });
    }
    Ok(())
}

fn scan_directory(root: &Path, directory: &Path, analysis: &mut Analysis) -> Result<(), InitError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|source| InitError::Io {
            operation: "read repository directory",
            path: relative_display(root, directory),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| InitError::Io {
            operation: "enumerate repository directory",
            path: relative_display(root, directory),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        analysis.entries = analysis.entries.saturating_add(1);
        if analysis.entries > MAX_INIT_SCAN_ENTRIES {
            return Err(InitError::TooManyEntries {
                limit: MAX_INIT_SCAN_ENTRIES,
            });
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| InitError::Io {
            operation: "inspect repository entry",
            path: relative_display(root, &path),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let relative = relative_display(root, &path);
            if should_skip_directory(&entry.file_name().to_string_lossy()) {
                analysis.skipped.insert(relative);
                continue;
            }
            scan_directory(root, &path, analysis)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let relative_parent = path
            .parent()
            .and_then(|parent| parent.strip_prefix(root).ok())
            .unwrap_or(Path::new(""))
            .to_path_buf();
        match entry.file_name().to_string_lossy().as_ref() {
            "Cargo.toml" => {
                analysis.ecosystems.insert(Ecosystem::Rust);
                analysis.subsystems.insert(relative_parent);
            }
            "package.json" => {
                analysis.ecosystems.insert(Ecosystem::TypeScript);
                analysis.subsystems.insert(relative_parent);
            }
            "pyproject.toml" | "setup.py" | "setup.cfg" => {
                analysis.ecosystems.insert(Ecosystem::Python);
                analysis.subsystems.insert(relative_parent);
            }
            _ => {}
        }
    }
    Ok(())
}

fn detect_root_commands(root: &Path, analysis: &mut Analysis) -> Result<(), InitError> {
    if root.join("Cargo.toml").is_file() {
        analysis
            .test_commands
            .insert("cargo test --workspace".to_owned());
        analysis
            .check_commands
            .insert("cargo fmt --all -- --check".to_owned());
        analysis
            .check_commands
            .insert("cargo clippy --workspace --all-targets -- -D warnings".to_owned());
    }
    if root.join("package.json").is_file() {
        let package = read_marker(&root.join("package.json"))?;
        let parsed: Value =
            serde_json::from_slice(&package).map_err(|_| InitError::MalformedMarker {
                path: PathBuf::from("package.json"),
            })?;
        let manager = if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
            "bun"
        } else if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else {
            "npm"
        };
        let scripts = parsed.get("scripts").and_then(Value::as_object);
        if scripts.is_some_and(|scripts| scripts.contains_key("test")) {
            analysis.test_commands.insert(format!("{manager} test"));
        }
        for script in ["typecheck", "lint"] {
            if scripts.is_some_and(|scripts| scripts.contains_key(script)) {
                analysis
                    .check_commands
                    .insert(format!("{manager} run {script}"));
            }
        }
    }
    if root.join("pyproject.toml").is_file()
        || root.join("setup.py").is_file()
        || root.join("setup.cfg").is_file()
    {
        let test_command = if marker_contains(&root.join("tox.ini"), b"-m unittest discover")? {
            "python -m unittest discover"
        } else {
            "python -m pytest"
        };
        analysis.test_commands.insert(test_command.to_owned());
        if root.join("ruff.toml").is_file()
            || marker_contains(&root.join("pyproject.toml"), b"[tool.ruff")?
        {
            analysis
                .check_commands
                .insert("python -m ruff check .".to_owned());
        }
    }
    Ok(())
}

fn marker_contains(path: &Path, needle: &[u8]) -> Result<bool, InitError> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(read_marker(path)?
        .windows(needle.len())
        .any(|window| window == needle))
}

fn read_marker(path: &Path) -> Result<Vec<u8>, InitError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| InitError::Io {
        operation: "inspect repository marker",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(InitError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > MAX_MARKER_BYTES {
        return Err(InitError::MarkerTooLarge {
            path: path.to_path_buf(),
            bytes: metadata.len(),
            limit: MAX_MARKER_BYTES,
        });
    }
    let file = File::open(path).map_err(|source| InitError::Io {
        operation: "open repository marker",
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_MARKER_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| InitError::Io {
            operation: "read repository marker",
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MARKER_BYTES {
        return Err(InitError::MarkerTooLarge {
            path: path.to_path_buf(),
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            limit: MAX_MARKER_BYTES,
        });
    }
    Ok(bytes)
}

fn render_root(analysis: &Analysis, deep_paths: &[PathBuf]) -> String {
    let mut output = String::from(
        "# Repository instructions\n\nGenerated by Rottweiler from repository metadata. Review and edit this human-owned file.\n\n",
    );
    output.push_str("## Stack\n\n");
    if analysis.ecosystems.is_empty() {
        output.push_str("- No recognized build-system marker was found. Inspect the repository before choosing commands.\n");
    } else {
        for ecosystem in &analysis.ecosystems {
            let _ = writeln!(output, "- {}", ecosystem.label());
        }
    }
    render_commands(&mut output, "Test commands", &analysis.test_commands);
    render_commands(
        &mut output,
        "Checks before completion",
        &analysis.check_commands,
    );
    output.push_str("\n## Working conventions\n\n- Inspect nearby code and nested AGENTS.md before editing.\n- Keep changes scoped and run the narrowest relevant test first.\n- Do not edit vendored or generated output directly.\n");
    if !deep_paths.is_empty() {
        output.push_str("\n## Subsystem instructions\n\n");
        for path in deep_paths {
            let _ = writeln!(output, "- `{}/AGENTS.md`", slash_path(path));
        }
    }
    output
}

fn render_subsystem(root: &Path, relative: &Path) -> String {
    let directory = root.join(relative);
    let mut markers = Vec::new();
    for name in [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
    ] {
        if directory.join(name).is_file() {
            markers.push(name);
        }
    }
    let scope = slash_path(relative);
    format!(
        "# Subsystem instructions: {scope}\n\nScope: `{scope}/**`. These instructions refine the repository root instructions.\n\n## Build metadata\n\n{}\n\n## Conventions\n\n- Keep changes within this subsystem unless an interface requires a coordinated edit.\n- Run this subsystem's declared scripts or tests before the repository-wide checks.\n- Treat generated and vendored directories as read-only.\n",
        if markers.is_empty() {
            "- No local build marker; inherit root commands.".to_owned()
        } else {
            markers
                .into_iter()
                .map(|marker| format!("- `{marker}`"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    )
}

fn render_commands(output: &mut String, title: &str, commands: &BTreeSet<String>) {
    let _ = write!(output, "\n## {title}\n\n");
    if commands.is_empty() {
        output.push_str("- No command inferred; inspect project documentation first.\n");
    } else {
        for command in commands {
            let _ = writeln!(output, "- `{command}`");
        }
    }
}

fn ensure_budget(path: &Path, content: &str, budget: usize) -> Result<(), InitError> {
    if content.len() > budget {
        return Err(InitError::GeneratedFileTooLarge {
            path: path.to_path_buf(),
            bytes: content.len(),
            limit: budget,
        });
    }
    Ok(())
}

fn should_skip_directory(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | ".venv"
            | "venv"
            | "node_modules"
            | "target"
            | "vendor"
            | "vendored"
            | "dist"
            | "build"
            | "out"
            | "coverage"
            | "__pycache__"
            | "generated"
            | ".generated"
    )
}

fn relative_display(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Safe `/init` planning or persistence failure.
#[derive(Debug, Error)]
pub enum InitError {
    /// Workspace root must be a directory.
    #[error("initialization workspace is not a directory")]
    WorkspaceNotDirectory,
    /// Per-file budget must be non-zero.
    #[error("initialization file budget must be greater than zero")]
    InvalidBudget,
    /// A symlink, special file, or escaping path was encountered.
    #[error("initialization encountered an unsafe path: {path}")]
    UnsafePath { path: PathBuf },
    /// Human-owned instructions are never silently replaced.
    #[error("instruction file already exists: {path}")]
    ExistingInstructions { path: PathBuf },
    /// Repository traversal is explicitly bounded.
    #[error("repository analysis exceeded {limit} entries")]
    TooManyEntries { limit: usize },
    /// Deep initialization is explicitly bounded.
    #[error("repository contains {count} subsystems; limit is {limit}")]
    TooManySubsystems { count: usize, limit: usize },
    /// Marker file exceeded its read budget.
    #[error("repository marker {path} is {bytes} bytes; limit is {limit}")]
    MarkerTooLarge {
        path: PathBuf,
        bytes: u64,
        limit: u64,
    },
    /// A structured marker could not be parsed.
    #[error("repository marker is malformed: {path}")]
    MalformedMarker { path: PathBuf },
    /// Generated instructions exceeded the caller's per-file budget.
    #[error("generated instruction file {path} is {bytes} bytes; limit is {limit}")]
    GeneratedFileTooLarge {
        path: PathBuf,
        bytes: usize,
        limit: usize,
    },
    /// This platform cannot provide race-safe, descriptor-relative creation.
    #[error("secure instruction creation is unsupported on this platform")]
    SecureCreationUnsupported,
    /// Sanitized filesystem failure.
    #[error("failed to {operation} at {path}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(all(test, unix))]
mod tests {
    use std::{collections::BTreeMap, fs, os::unix::fs::symlink, path::PathBuf};

    use tempfile::tempdir;

    use super::{InitPlan, apply_init_plan_unix};

    fn must<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
        result.unwrap_or_else(|error| panic!("{context}: {error}"))
    }

    #[test]
    fn parent_symlink_swap_fails_closed_and_rolls_back_pinned_files() {
        let workspace = must(tempdir(), "create workspace tempdir");
        let outside = must(tempdir(), "create outside tempdir");
        must(
            fs::create_dir(workspace.path().join("package")),
            "create package",
        );
        let root = must(fs::canonicalize(workspace.path()), "canonicalize workspace");
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("AGENTS.md"), "root".to_owned());
        files.insert(PathBuf::from("package/AGENTS.md"), "package".to_owned());
        let plan = InitPlan {
            root: root.clone(),
            files,
            skipped_directories: Vec::new(),
        };

        let result = apply_init_plan_unix(&plan, || {
            must(
                fs::rename(root.join("package"), root.join("package-pinned")),
                "move preflighted package",
            );
            must(
                symlink(outside.path(), root.join("package")),
                "swap package for symlink",
            );
        });

        assert!(result.is_err());
        assert!(!root.join("AGENTS.md").exists());
        assert!(!root.join("package-pinned/AGENTS.md").exists());
        assert!(!outside.path().join("AGENTS.md").exists());
    }
}
