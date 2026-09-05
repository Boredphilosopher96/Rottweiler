mod formats;
use formats::{
    parse_json, render_claude_hooks, render_claude_mcp, render_opencode_mcp, shift_claude_args,
    strip_jsonc,
};

use std::cell::Cell;
use std::fs;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use clap::ValueEnum;
use miette::{IntoDiagnostic, Result, miette};
use serde::Serialize;
use serde_json::Value;

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum ImportSource {
    Claude,
    Opencode,
    Pi,
}

#[derive(Clone, Debug)]
pub(crate) struct ImportOptions {
    pub(crate) source: ImportSource,
    pub(crate) source_root: PathBuf,
    pub(crate) target_root: PathBuf,
    pub(crate) dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImportStatus {
    Planned,
    Created,
    Unchanged,
    Conflict,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ImportItem {
    pub(crate) kind: String,
    pub(crate) target: String,
    pub(crate) status: ImportStatus,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ImportReport {
    pub(crate) source: String,
    pub(crate) dry_run: bool,
    pub(crate) items: Vec<ImportItem>,
}

#[derive(Clone)]
struct PlannedWrite {
    kind: &'static str,
    relative: PathBuf,
    bytes: Vec<u8>,
}

pub(crate) fn run(options: &ImportOptions) -> Result<ImportReport> {
    let source = SourceTree::open(&options.source_root)?;
    let mut writes = Vec::new();
    let mut diagnostics = Vec::new();
    match options.source {
        ImportSource::Claude => import_claude(&source, &mut writes, &mut diagnostics)?,
        ImportSource::Opencode => import_opencode(&source, &mut writes, &mut diagnostics)?,
        ImportSource::Pi => import_pi(&source, &mut writes, &mut diagnostics)?,
    }
    writes.sort_by(|left, right| left.relative.cmp(&right.relative));
    writes.dedup_by(|left, right| left.relative == right.relative && left.bytes == right.bytes);
    if writes
        .windows(2)
        .any(|pair| pair[0].relative == pair[1].relative)
    {
        return Err(miette!(
            "multiple source artifacts map to the same import target"
        ));
    }
    if writes.len() > MAX_FILES
        || writes.iter().map(|write| write.bytes.len()).sum::<usize>() > MAX_TOTAL_BYTES
    {
        return Err(miette!(
            "import plan exceeds the bounded file or byte limit"
        ));
    }
    let target_root = prepare_target_root(&options.target_root, options.dry_run)?;
    let mut items = diagnostics;
    let mut inspected = Vec::with_capacity(writes.len());
    for write in writes {
        let display = write.relative.to_string_lossy().into_owned();
        let state = inspect_target(&target_root, &write.relative, &write.bytes)?;
        inspected.push((write, display, state));
    }
    let mut created = Vec::new();
    for (write, display, state) in inspected {
        match state {
            TargetState::Missing if options.dry_run => items.push(item(
                write.kind,
                display,
                ImportStatus::Planned,
                "create new file",
            )),
            TargetState::Missing => {
                let created_target =
                    match create_target(&target_root, &write.relative, &write.bytes) {
                        Ok(created_target) => created_target,
                        Err(error) => {
                            rollback_created_targets(&created);
                            return Err(error);
                        }
                    };
                created.push(created_target);
                items.push(item(write.kind, display, ImportStatus::Created, "created"));
            }
            TargetState::Identical => items.push(item(
                write.kind,
                display,
                ImportStatus::Unchanged,
                "already identical",
            )),
            TargetState::Conflict => items.push(item(
                write.kind,
                display,
                ImportStatus::Conflict,
                "existing file preserved",
            )),
        }
    }
    items.sort_by(|left, right| {
        left.target
            .cmp(&right.target)
            .then(left.kind.cmp(&right.kind))
    });
    Ok(ImportReport {
        source: format!("{:?}", options.source).to_ascii_lowercase(),
        dry_run: options.dry_run,
        items,
    })
}

#[cfg(unix)]
struct CreatedTarget {
    parent: std::os::fd::OwnedFd,
    name: std::ffi::OsString,
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
struct CreatedTarget {
    path: PathBuf,
}

#[cfg(unix)]
fn rollback_created_targets(created: &[CreatedTarget]) {
    for target in created.iter().rev() {
        let Ok(stat) = rustix::fs::statat(
            &target.parent,
            &target.name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) else {
            continue;
        };
        if crate::runtime_paths::rustix_device_id(stat.st_dev) == Some(target.device)
            && stat.st_ino == target.inode
            && stat.st_nlink == 1
            && rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        {
            let _ =
                rustix::fs::unlinkat(&target.parent, &target.name, rustix::fs::AtFlags::empty());
        }
    }
}

#[cfg(not(unix))]
fn rollback_created_targets(created: &[CreatedTarget]) {
    for target in created.iter().rev() {
        let _ = fs::remove_file(&target.path);
    }
}

fn import_claude(
    source: &SourceTree,
    writes: &mut Vec<PlannedWrite>,
    diagnostics: &mut Vec<ImportItem>,
) -> Result<()> {
    if let Some(bytes) = source.first_file(&["CLAUDE.md", ".claude/CLAUDE.md"])? {
        writes.push(write("instructions", "AGENTS.md", bytes));
    }
    copy_markdown_dir(
        source,
        &[".claude/commands", "commands"],
        ".agents/commands",
        true,
        writes,
    )?;
    copy_tree(
        source,
        &[".claude/skills", "skills"],
        ".agents/skills",
        writes,
    )?;
    copy_markdown_dir(
        source,
        &[".claude/memory", "memory"],
        ".agents/memory",
        false,
        writes,
    )?;
    if let Some(bytes) = source.first_file(&[".mcp.json", ".claude/.mcp.json"])? {
        let value = parse_json(&bytes, "Claude MCP config")?;
        if let Some(rendered) = render_claude_mcp(&value, diagnostics)? {
            writes.push(write("mcp", ".agents/mcp.toml", rendered.into_bytes()));
        }
    }
    if let Some(bytes) = source.first_file(&[".claude/settings.json", "settings.json"])? {
        let value = parse_json(&bytes, "Claude settings")?;
        if let Some(rendered) = render_claude_hooks(&value, diagnostics)? {
            writes.push(write("hooks", ".agents/hooks.toml", rendered.into_bytes()));
        }
    }
    for path in [".claude/plugins", "plugins", ".claude.json"] {
        if source.exists(path)? {
            diagnostics.push(item(
                "executable",
                path,
                ImportStatus::Unsupported,
                "credentials, runtime state, and executable plugins are never imported",
            ));
        }
    }
    Ok(())
}

fn import_opencode(
    source: &SourceTree,
    writes: &mut Vec<PlannedWrite>,
    diagnostics: &mut Vec<ImportItem>,
) -> Result<()> {
    if let Some(bytes) = source.first_file(&[
        "AGENTS.md",
        ".opencode/AGENTS.md",
        "CLAUDE.md",
        ".opencode/CLAUDE.md",
    ])? {
        writes.push(write("instructions", "AGENTS.md", bytes));
    }
    copy_markdown_dir(
        source,
        &[".opencode/commands", "commands"],
        ".agents/commands",
        false,
        writes,
    )?;
    copy_tree(
        source,
        &[".opencode/skills", "skills"],
        ".agents/skills",
        writes,
    )?;
    if let Some(bytes) = source.first_file(&[
        "opencode.json",
        "opencode.jsonc",
        ".opencode/opencode.json",
        ".opencode/opencode.jsonc",
    ])? {
        let cleaned = strip_jsonc(&bytes)?;
        let value = parse_json(cleaned.as_bytes(), "OpenCode config")?;
        render_opencode_commands(&value, writes, diagnostics);
        if let Some(rendered) = render_opencode_mcp(&value, diagnostics)? {
            writes.push(write("mcp", ".agents/mcp.toml", rendered.into_bytes()));
        }
        if value.get("plugin").is_some() || value.get("plugins").is_some() {
            diagnostics.push(item(
                "executable",
                "plugins",
                ImportStatus::Unsupported,
                "OpenCode plugins are executable and are never imported",
            ));
        }
    }
    for path in ["auth.json", ".opencode/auth.json", "mcp-auth.json"] {
        if source.exists(path)? {
            diagnostics.push(item(
                "credential",
                path,
                ImportStatus::Unsupported,
                "authentication data is never read or imported",
            ));
        }
    }
    Ok(())
}

fn import_pi(
    source: &SourceTree,
    writes: &mut Vec<PlannedWrite>,
    diagnostics: &mut Vec<ImportItem>,
) -> Result<()> {
    copy_markdown_dir(
        source,
        &[".pi/agent/prompts", ".pi/prompts", "prompts"],
        ".agents/commands",
        false,
        writes,
    )?;
    copy_tree(
        source,
        &[".pi/agent/skills", ".pi/skills", "skills"],
        ".agents/skills",
        writes,
    )?;
    for path in [".pi/extensions", "extensions", ".pi/agent/extensions"] {
        if source.exists(path)? {
            diagnostics.push(item(
                "executable",
                path,
                ImportStatus::Unsupported,
                "pi extensions require a plugin-SDK port and are never executed or copied",
            ));
        }
    }
    diagnostics.push(item(
        "mcp",
        ".agents/mcp.toml",
        ImportStatus::Unsupported,
        "pi has no core MCP configuration to import",
    ));
    Ok(())
}

fn copy_markdown_dir(
    source: &SourceTree,
    candidates: &[&str],
    target: &str,
    shift_claude_arguments: bool,
    writes: &mut Vec<PlannedWrite>,
) -> Result<()> {
    let Some(root) = first_source_directory(source, candidates)? else {
        return Ok(());
    };
    for (relative, mut bytes) in source.markdown_files_recursive(root)? {
        let relative = if target.ends_with("commands") {
            flattened_command_path(&relative)?
        } else {
            relative
        };
        if target.ends_with("commands") {
            bytes = normalize_command_markdown(&relative, bytes, shift_claude_arguments)?;
        } else if shift_claude_arguments {
            let text = String::from_utf8(bytes)
                .map_err(|_| miette!("Claude command is not valid UTF-8"))?;
            bytes = shift_claude_args(&text).into_bytes();
        }
        writes.push(PlannedWrite {
            kind: if target.ends_with("commands") {
                "command"
            } else {
                "memory"
            },
            relative: Path::new(target).join(relative),
            bytes,
        });
    }
    Ok(())
}

fn flattened_command_path(relative: &Path) -> Result<PathBuf> {
    let parts = relative
        .with_extension("")
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .filter(|value| valid_artifact_name(value))
                .map(str::to_owned)
                .ok_or_else(|| miette!("command name is not a supported portable name")),
            _ => Err(miette!("command path is unsafe")),
        })
        .collect::<Result<Vec<_>>>()?;
    if parts.is_empty() {
        return Err(miette!("command path has no name"));
    }
    let name = parts.join("-");
    if !valid_artifact_name(&name) {
        return Err(miette!("command name is not supported by Rottweiler"));
    }
    Ok(PathBuf::from(format!("{name}.md")))
}

fn normalize_command_markdown(
    relative: &Path,
    bytes: Vec<u8>,
    shift_claude_arguments: bool,
) -> Result<Vec<u8>> {
    let mut text = String::from_utf8(bytes).map_err(|_| miette!("command is not valid UTF-8"))?;
    if shift_claude_arguments {
        text = shift_claude_args(&text);
    }
    let name = relative
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| miette!("command has no portable name"))?;
    if let Some(after_open) = text.strip_prefix("---\n") {
        let Some(close) = after_open.find("\n---\n") else {
            return Err(miette!("command frontmatter is unterminated"));
        };
        let frontmatter = &after_open[..close];
        if frontmatter
            .lines()
            .any(|line| line.trim_start().starts_with("description:"))
        {
            return Ok(text.into_bytes());
        }
        let mut output = String::with_capacity(text.len() + name.len() + 32);
        output.push_str("---\n");
        output.push_str("description: Imported command ");
        output.push_str(name);
        output.push('\n');
        output.push_str(after_open);
        return Ok(output.into_bytes());
    }
    Ok(format!("---\ndescription: Imported command {name}\n---\n{text}").into_bytes())
}

fn render_opencode_commands(
    value: &Value,
    writes: &mut Vec<PlannedWrite>,
    diagnostics: &mut Vec<ImportItem>,
) {
    let Some(commands) = value.get("command").and_then(Value::as_object) else {
        return;
    };
    for (name, entry) in commands {
        if !valid_artifact_name(name) {
            diagnostics.push(item(
                "command",
                name,
                ImportStatus::Unsupported,
                "command name is not supported by Rottweiler",
            ));
            continue;
        }
        let Some(template) = entry.get("template").and_then(Value::as_str) else {
            diagnostics.push(item(
                "command",
                name,
                ImportStatus::Unsupported,
                "inline command has no string template",
            ));
            continue;
        };
        if entry.get("agent").is_some() {
            diagnostics.push(item(
                "command",
                name,
                ImportStatus::Unsupported,
                "OpenCode command agent selection is not imported",
            ));
        }
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .filter(|value| !value.contains(['\r', '\n']))
            .unwrap_or("Imported OpenCode command");
        let model = entry
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.contains(['\r', '\n']));
        let mut rendered = format!("---\ndescription: {description}\n");
        if let Some(model) = model {
            rendered.push_str("model: ");
            rendered.push_str(model);
            rendered.push('\n');
        }
        rendered.push_str("---\n");
        rendered.push_str(template);
        writes.push(write(
            "command",
            format!(".agents/commands/{name}.md"),
            rendered.into_bytes(),
        ));
    }
}

fn valid_artifact_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
}

fn copy_tree(
    source: &SourceTree,
    candidates: &[&str],
    target: &str,
    writes: &mut Vec<PlannedWrite>,
) -> Result<()> {
    let Some(root) = first_source_directory(source, candidates)? else {
        return Ok(());
    };
    for (relative, bytes) in source.files_recursive(root)? {
        writes.push(PlannedWrite {
            kind: "skill",
            relative: Path::new(target).join(relative),
            bytes,
        });
    }
    Ok(())
}

fn first_source_directory<'a>(
    source: &SourceTree,
    candidates: &'a [&str],
) -> Result<Option<&'a str>> {
    for candidate in candidates {
        if source.is_dir(candidate)? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn write(kind: &'static str, relative: impl Into<PathBuf>, bytes: Vec<u8>) -> PlannedWrite {
    PlannedWrite {
        kind,
        relative: relative.into(),
        bytes,
    }
}

fn item(
    kind: impl Into<String>,
    target: impl Into<String>,
    status: ImportStatus,
    detail: impl Into<String>,
) -> ImportItem {
    ImportItem {
        kind: kind.into(),
        target: target.into(),
        status,
        detail: detail.into(),
    }
}

#[derive(Clone, Copy)]
enum TargetState {
    Missing,
    Identical,
    Conflict,
}

struct TargetRoot {
    #[cfg(not(unix))]
    path: PathBuf,
    #[cfg(unix)]
    descriptor: std::os::fd::OwnedFd,
}

#[cfg(unix)]
fn inspect_target(root: &TargetRoot, relative: &Path, expected: &[u8]) -> Result<TargetState> {
    use std::os::fd::AsFd as _;
    validate_relative(relative)?;
    let mut directory = root
        .descriptor
        .as_fd()
        .try_clone_to_owned()
        .into_diagnostic()?;
    for component in relative.parent().into_iter().flat_map(Path::components) {
        let Component::Normal(name) = component else {
            return Err(miette!("unsafe import target path"));
        };
        directory = match rustix::fs::openat(
            &directory,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(rustix::io::Errno::NOENT) => {
                return Ok(TargetState::Missing);
            }
            Err(error) => return Err(std::io::Error::from(error)).into_diagnostic(),
        };
    }
    let name = relative
        .file_name()
        .ok_or_else(|| miette!("import target has no file name"))?;
    let descriptor = match rustix::fs::openat(
        &directory,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(TargetState::Missing),
        Err(error) => return Err(std::io::Error::from(error)).into_diagnostic(),
    };
    let stat = rustix::fs::fstat(&descriptor)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    if stat.st_nlink != 1 || !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(miette!("import target is not a safe regular file"));
    }
    if usize::try_from(stat.st_size).unwrap_or(usize::MAX) > MAX_FILE_BYTES {
        return Ok(TargetState::Conflict);
    }
    let mut bytes = Vec::with_capacity(expected.len().min(MAX_FILE_BYTES));
    std::fs::File::from(descriptor)
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    Ok(if bytes == expected {
        TargetState::Identical
    } else {
        TargetState::Conflict
    })
}

#[cfg(not(unix))]
fn inspect_target(root: &TargetRoot, relative: &Path, expected: &[u8]) -> Result<TargetState> {
    validate_relative(relative)?;
    let path = root.path.join(relative);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TargetState::Missing),
        Err(error) => Err(error).into_diagnostic(),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(miette!("import target is not a regular file"))
        }
        Ok(metadata) if usize::try_from(metadata.len()).unwrap_or(usize::MAX) > MAX_FILE_BYTES => {
            Ok(TargetState::Conflict)
        }
        Ok(_) => Ok(if fs::read(path).into_diagnostic()? == expected {
            TargetState::Identical
        } else {
            TargetState::Conflict
        }),
    }
}

fn prepare_target_root(root: &Path, _dry_run: bool) -> Result<TargetRoot> {
    if !root.exists() {
        return Err(miette!("import target project root must already exist"));
    }
    let metadata = fs::symlink_metadata(root).into_diagnostic()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(miette!("import target root must be a real directory"));
    }
    #[cfg(unix)]
    let descriptor = rustix::fs::open(
        root,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let opened = rustix::fs::fstat(&descriptor)
            .map_err(std::io::Error::from)
            .into_diagnostic()?;
        if Some(metadata.dev()) != crate::runtime_paths::rustix_device_id(opened.st_dev)
            || metadata.ino() != opened.st_ino
        {
            return Err(miette!("import target root changed during inspection"));
        }
    }
    Ok(TargetRoot {
        #[cfg(not(unix))]
        path: root.to_path_buf(),
        #[cfg(unix)]
        descriptor,
    })
}

#[cfg(unix)]
fn create_target(root: &TargetRoot, relative: &Path, bytes: &[u8]) -> Result<CreatedTarget> {
    use std::os::fd::AsFd as _;
    validate_relative(relative)?;
    let mut directory = root
        .descriptor
        .as_fd()
        .try_clone_to_owned()
        .into_diagnostic()?;
    for component in relative.parent().into_iter().flat_map(Path::components) {
        if let Component::Normal(name) = component {
            match rustix::fs::mkdirat(&directory, name, rustix::fs::Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(std::io::Error::from(error)).into_diagnostic(),
            }
            directory = rustix::fs::openat(
                &directory,
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)
            .into_diagnostic()?;
        }
    }
    let name = relative
        .file_name()
        .ok_or_else(|| miette!("import target has no file name"))?;
    let descriptor = rustix::fs::openat(
        &directory,
        name,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let stat = rustix::fs::fstat(&descriptor)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    if stat.st_nlink != 1 || !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(miette!("created import target is unsafe"));
    }
    let created = CreatedTarget {
        parent: directory,
        name: name.to_os_string(),
        device: crate::runtime_paths::rustix_device_id(stat.st_dev).unwrap_or(u64::MAX),
        inode: stat.st_ino,
    };
    let mut file = std::fs::File::from(descriptor);
    let result = (|| -> Result<()> {
        file.write_all(bytes).into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        rustix::fs::fsync(&created.parent)
            .map_err(std::io::Error::from)
            .into_diagnostic()
    })();
    if let Err(error) = result {
        rollback_created_targets(std::slice::from_ref(&created));
        return Err(error);
    }
    Ok(created)
}

#[cfg(not(unix))]
fn create_target(root: &TargetRoot, relative: &Path, bytes: &[u8]) -> Result<CreatedTarget> {
    validate_relative(relative)?;
    let target = root.path.join(relative);
    fs::create_dir_all(
        target
            .parent()
            .ok_or_else(|| miette!("target has no parent"))?,
    )
    .into_diagnostic()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()?;
    Ok(CreatedTarget {
        path: root.path.join(relative),
    })
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(miette!("unsafe import-relative path"));
    }
    Ok(())
}

struct SourceTree {
    #[cfg(not(unix))]
    root: PathBuf,
    #[cfg(unix)]
    descriptor: std::os::fd::OwnedFd,
    files_read: Cell<usize>,
    bytes_read: Cell<usize>,
}

impl SourceTree {
    fn open(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path).into_diagnostic()?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(miette!("import source must be a real directory"));
        }
        let root = fs::canonicalize(path).into_diagnostic()?;
        #[cfg(unix)]
        let descriptor = rustix::fs::open(
            &root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
        Ok(Self {
            #[cfg(not(unix))]
            root,
            #[cfg(unix)]
            descriptor,
            files_read: Cell::new(0),
            bytes_read: Cell::new(0),
        })
    }
    fn first_file(&self, paths: &[&str]) -> Result<Option<Vec<u8>>> {
        for path in paths {
            if let Some(bytes) = self.read(path)? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }
    fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.kind(path)?.is_some())
    }
    fn is_dir(&self, path: &str) -> Result<bool> {
        Ok(self.kind(path)? == Some(FileKind::Directory))
    }
    fn kind(&self, path: &str) -> Result<Option<FileKind>> {
        secure_kind(self, Path::new(path))
    }
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
        secure_read(self, Path::new(path))
    }
    fn files_recursive(&self, root: &str) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        secure_walk(self, Path::new(root), None)
    }
    fn markdown_files_recursive(&self, root: &str) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        secure_walk(self, Path::new(root), Some("md"))
    }
    fn charge_read(&self, bytes: usize) -> Result<()> {
        let files = self.files_read.get().saturating_add(1);
        let total = self.bytes_read.get().saturating_add(bytes);
        if files > MAX_FILES || total > MAX_TOTAL_BYTES {
            return Err(miette!("import source exceeds the aggregate read budget"));
        }
        self.files_read.set(files);
        self.bytes_read.set(total);
        Ok(())
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FileKind {
    File,
    Directory,
}

#[cfg(unix)]
fn open_parent(
    tree: &SourceTree,
    path: &Path,
) -> Result<Option<(std::os::fd::OwnedFd, std::ffi::OsString)>> {
    use std::os::fd::AsFd as _;
    validate_relative(path)?;
    let mut directory = tree
        .descriptor
        .as_fd()
        .try_clone_to_owned()
        .into_diagnostic()?;
    let name = path
        .file_name()
        .ok_or_else(|| miette!("source path has no name"))?
        .to_os_string();
    if let Some(parent) = path.parent() {
        for component in parent.components() {
            if let Component::Normal(part) = component {
                directory = match rustix::fs::openat(
                    &directory,
                    part,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                ) {
                    Ok(directory) => directory,
                    Err(rustix::io::Errno::NOENT) => return Ok(None),
                    Err(error) => return Err(std::io::Error::from(error)).into_diagnostic(),
                };
            }
        }
    }
    Ok(Some((directory, name)))
}

#[cfg(unix)]
fn secure_kind(tree: &SourceTree, path: &Path) -> Result<Option<FileKind>> {
    let Some((parent, name)) = open_parent(tree, path)? else {
        return Ok(None);
    };
    let stat = match rustix::fs::statat(&parent, &name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(std::io::Error::from(error)).into_diagnostic(),
    };
    let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    if kind.is_symlink() || (!kind.is_file() && !kind.is_dir()) {
        return Err(miette!("import source contains a symlink or special file"));
    }
    if kind.is_file() && stat.st_nlink != 1 {
        return Err(miette!("import source file has multiple hard links"));
    }
    Ok(Some(if kind.is_dir() {
        FileKind::Directory
    } else {
        FileKind::File
    }))
}

#[cfg(unix)]
fn secure_read(tree: &SourceTree, path: &Path) -> Result<Option<Vec<u8>>> {
    if secure_kind(tree, path)? != Some(FileKind::File) {
        return Ok(None);
    }
    let (parent, name) =
        open_parent(tree, path)?.ok_or_else(|| miette!("import source changed during read"))?;
    let fd = rustix::fs::openat(
        &parent,
        &name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let stat = rustix::fs::fstat(&fd)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    if stat.st_nlink != 1
        || !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        || usize::try_from(stat.st_size).unwrap_or(usize::MAX) > MAX_FILE_BYTES
    {
        return Err(miette!("import source file exceeds safety bounds"));
    }
    let mut bytes = Vec::new();
    let file = std::fs::File::from(fd);
    file.try_clone()
        .into_diagnostic()?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(miette!("import source file exceeds safety bounds"));
    }
    let after = rustix::fs::fstat(&file)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    ensure_unchanged_stat(&stat, &after)?;
    tree.charge_read(bytes.len())?;
    Ok(Some(bytes))
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn secure_walk(
    tree: &SourceTree,
    root: &Path,
    extension: Option<&str>,
) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    fn visit(
        tree: &SourceTree,
        directory: &std::os::fd::OwnedFd,
        relative: &Path,
        extension: Option<&str>,
        output: &mut Vec<(PathBuf, Vec<u8>)>,
    ) -> Result<()> {
        use std::os::fd::AsFd as _;
        use std::os::unix::ffi::OsStrExt as _;
        if output.len() >= MAX_FILES {
            return Err(miette!("import source has too many files"));
        }
        let mut entries =
            rustix::fs::Dir::read_from(directory.as_fd().try_clone_to_owned().into_diagnostic()?)
                .map_err(std::io::Error::from)
                .into_diagnostic()?;
        let mut names = Vec::new();
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(std::io::Error::from).into_diagnostic()?;
            let name = entry.file_name();
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                names.push(std::ffi::OsStr::from_bytes(name.to_bytes()).to_os_string());
            }
        }
        names.sort();
        for name in names {
            let child = relative.join(&name);
            let stat = rustix::fs::statat(directory, &name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)
                .into_diagnostic()?;
            let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            if kind.is_symlink() || (!kind.is_file() && !kind.is_dir()) {
                return Err(miette!("import source contains a symlink or special file"));
            }
            if kind.is_dir() {
                let child_directory = rustix::fs::openat(
                    directory,
                    &name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(std::io::Error::from)
                .into_diagnostic()?;
                visit(tree, &child_directory, &child, extension, output)?;
                continue;
            }
            if stat.st_nlink != 1 {
                return Err(miette!("import source file has multiple hard links"));
            }
            if sensitive_import_path(&child) {
                return Err(miette!(
                    "import source contains a credential or authentication file in a copied tree"
                ));
            }
            if extension.is_some_and(|expected| {
                child.extension().and_then(|value| value.to_str()) != Some(expected)
            }) {
                continue;
            }
            let descriptor = rustix::fs::openat(
                directory,
                &name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)
            .into_diagnostic()?;
            let opened = rustix::fs::fstat(&descriptor)
                .map_err(std::io::Error::from)
                .into_diagnostic()?;
            if opened.st_nlink != 1
                || !rustix::fs::FileType::from_raw_mode(opened.st_mode).is_file()
                || usize::try_from(opened.st_size).unwrap_or(usize::MAX) > MAX_FILE_BYTES
                || opened.st_dev != stat.st_dev
                || opened.st_ino != stat.st_ino
            {
                return Err(miette!("import source file exceeds safety bounds"));
            }
            let length = usize::try_from(opened.st_size).unwrap_or(usize::MAX);
            let mut bytes = Vec::with_capacity(length);
            let file = std::fs::File::from(descriptor);
            file.try_clone()
                .into_diagnostic()?
                .take((MAX_FILE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .into_diagnostic()?;
            if bytes.len() > MAX_FILE_BYTES {
                return Err(miette!("import source file exceeds safety bounds"));
            }
            let after = rustix::fs::fstat(&file)
                .map_err(std::io::Error::from)
                .into_diagnostic()?;
            ensure_unchanged_stat(&opened, &after)?;
            tree.charge_read(bytes.len())?;
            output.push((child, bytes));
        }
        Ok(())
    }
    use std::os::fd::AsFd as _;
    validate_relative(root)?;
    let mut directory = tree
        .descriptor
        .as_fd()
        .try_clone_to_owned()
        .into_diagnostic()?;
    for component in root.components() {
        let Component::Normal(name) = component else {
            return Err(miette!("unsafe import source path"));
        };
        directory = match rustix::fs::openat(
            &directory,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
            Err(error) => return Err(std::io::Error::from(error)).into_diagnostic(),
        };
    }
    let mut output = Vec::new();
    visit(tree, &directory, Path::new(""), extension, &mut output)?;
    Ok(output)
}

#[cfg(unix)]
fn ensure_unchanged_stat(before: &rustix::fs::Stat, after: &rustix::fs::Stat) -> Result<()> {
    if after.st_dev != before.st_dev
        || after.st_ino != before.st_ino
        || after.st_size != before.st_size
        || after.st_mtime != before.st_mtime
        || after.st_mtime_nsec != before.st_mtime_nsec
        || after.st_ctime != before.st_ctime
        || after.st_ctime_nsec != before.st_ctime_nsec
    {
        return Err(miette!("import source changed while it was being read"));
    }
    Ok(())
}

fn sensitive_import_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                ".env" | "auth.json" | "auth.toml" | "credentials.json" | "mcp-auth.json"
            ) || name.to_ascii_lowercase().starts_with(".env.")
        })
}

#[cfg(not(unix))]
fn secure_kind(tree: &SourceTree, path: &Path) -> Result<Option<FileKind>> {
    let path = tree.root.join(path);
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).into_diagnostic(),
        Ok(meta) if meta.file_type().is_symlink() => Err(miette!("symlink source rejected")),
        Ok(meta) if meta.is_dir() => Ok(Some(FileKind::Directory)),
        Ok(meta) if meta.is_file() => Ok(Some(FileKind::File)),
        Ok(_) => Err(miette!("special source rejected")),
    }
}
#[cfg(not(unix))]
fn secure_read(tree: &SourceTree, path: &Path) -> Result<Option<Vec<u8>>> {
    if secure_kind(tree, path)? != Some(FileKind::File) {
        return Ok(None);
    }
    let bytes = fs::read(tree.root.join(path)).into_diagnostic()?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(miette!("source too large"));
    }
    tree.charge_read(bytes.len())?;
    Ok(Some(bytes))
}
#[cfg(not(unix))]
fn secure_walk(
    tree: &SourceTree,
    root: &Path,
    extension: Option<&str>,
) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut output = Vec::new();
    for entry in walkdir_fallback(&tree.root.join(root), Path::new(""))? {
        if sensitive_import_path(&entry) {
            return Err(miette!("credential file in copied import tree"));
        }
        if extension.is_some_and(|expected| {
            entry.extension().and_then(|value| value.to_str()) != Some(expected)
        }) {
            continue;
        }
        output.push((
            entry.clone(),
            secure_read(tree, &root.join(entry))?.ok_or_else(|| miette!("source changed"))?,
        ));
    }
    Ok(output)
}
#[cfg(not(unix))]
fn walkdir_fallback(root: &Path, relative: &Path) -> Result<Vec<PathBuf>> {
    let mut output = Vec::new();
    let mut entries = fs::read_dir(root)
        .into_diagnostic()?
        .collect::<std::result::Result<Vec<_>, _>>()
        .into_diagnostic()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let child = relative.join(entry.file_name());
        let meta = entry.file_type().into_diagnostic()?;
        if meta.is_symlink() {
            return Err(miette!("symlink source rejected"));
        }
        if meta.is_dir() {
            output.extend(walkdir_fallback(&entry.path(), &child)?);
        } else if meta.is_file() {
            output.push(child);
        }
    }
    Ok(output)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
