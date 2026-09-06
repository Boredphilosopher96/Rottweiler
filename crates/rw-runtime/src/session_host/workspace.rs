use super::*;

pub(super) fn safe_relative_path(value: &str) -> Result<PathBuf, HostError> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err(HostError::Query(
            "workspace path must be a non-empty normalized relative path".to_owned(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(HostError::Query(
                "workspace path must be a non-empty normalized relative path".to_owned(),
            ));
        };
        normalized.push(name);
    }
    if normalized.as_os_str().is_empty() {
        return Err(HostError::Query(
            "workspace path must be a non-empty normalized relative path".to_owned(),
        ));
    }
    Ok(normalized)
}

pub(super) fn split_virtual_path(value: &str) -> Result<(usize, PathBuf), HostError> {
    let normalized = safe_relative_path(value)?;
    let mut components = normalized.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Err(HostError::Query("workspace path is invalid".to_owned()));
    };
    if first != "@root" {
        return Ok((0, normalized));
    }
    let Some(Component::Normal(index)) = components.next() else {
        return Err(HostError::Query(
            "virtual workspace path must use @root/<index>/...".to_owned(),
        ));
    };
    let index = index
        .to_str()
        .and_then(|index| index.parse::<usize>().ok())
        .filter(|index| *index > 0)
        .ok_or_else(|| HostError::Query("workspace root index must be positive".to_owned()))?;
    let relative = components.fold(PathBuf::new(), |path, component| {
        path.join(component.as_os_str())
    });
    if relative.as_os_str().is_empty() {
        return Err(HostError::Query(
            "virtual workspace path must name a file".to_owned(),
        ));
    }
    Ok((index, relative))
}

#[cfg(unix)]
pub(super) fn search_workspaces(
    workspaces: &[PathBuf],
    query: &str,
    limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    let mut combined = Vec::new();
    let mut truncated = false;
    for (index, workspace) in workspaces.iter().enumerate() {
        let remaining = limit.saturating_sub(combined.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        let (mut matches, root_truncated) = search_workspace(workspace, query, remaining)?;
        if index > 0 {
            for item in &mut matches {
                item.path = format!("@root/{index}/{}", item.path);
            }
        }
        combined.extend(matches);
        truncated |= root_truncated;
        if truncated || combined.len() >= limit {
            break;
        }
    }
    combined.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((combined, truncated))
}

#[cfg(not(unix))]
pub(super) fn search_workspaces(
    _workspaces: &[PathBuf],
    _query: &str,
    _limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    Err(HostError::Query(
        "safe workspace search is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
pub(super) fn preview_file(
    workspace: &Path,
    relative: &Path,
    maximum: usize,
) -> Result<WorkspaceFilePreview, HostError> {
    let root = open_workspace_directory(workspace)?;
    let file = open_relative_regular_file(&root, relative)?;
    let stat = rustix::fs::fstat(&file)
        .map_err(|_| HostError::Query("workspace file metadata is unavailable".to_owned()))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(HostError::Query(
            "workspace preview accepts regular files only".to_owned(),
        ));
    }
    let total_bytes = usize::try_from(stat.st_size).unwrap_or(usize::MAX);
    if total_bytes > maximum {
        return Err(HostError::Query(
            "workspace file exceeds the preview byte limit".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(total_bytes.min(maximum));
    fs::File::from(file)
        .take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| HostError::Query("workspace file could not be read".to_owned()))?;
    if bytes.len() > maximum {
        return Err(HostError::Query(
            "workspace file exceeded the preview byte limit while reading".to_owned(),
        ));
    }
    if let Some(media_type) = workspace_image_media_type(&bytes) {
        return Ok(WorkspaceFilePreview {
            path: relative.to_string_lossy().into_owned(),
            media_type: media_type.to_owned(),
            data: AttachmentData::InlineBase64 {
                data: BASE64_STANDARD.encode(&bytes),
            },
            total_bytes: u64::try_from(total_bytes).unwrap_or(u64::MAX),
            truncated: false,
        });
    }
    if total_bytes > MAX_TEXT_PREVIEW_BYTES {
        return Err(HostError::Query(
            "text attachment exceeds the 1 MiB message limit".to_owned(),
        ));
    }
    if bytes.contains(&0) {
        return Err(HostError::Query(
            "this binary file type cannot be attached".to_owned(),
        ));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| HostError::Query("binary workspace files are not previewed".to_owned()))?;
    Ok(WorkspaceFilePreview {
        path: relative.to_string_lossy().into_owned(),
        media_type: "text/plain".to_owned(),
        data: AttachmentData::Text { content },
        total_bytes: u64::try_from(total_bytes).unwrap_or(u64::MAX),
        truncated: false,
    })
}

#[cfg(unix)]
pub(super) fn workspace_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(not(unix))]
pub(super) fn preview_file(
    _workspace: &Path,
    _relative: &Path,
    _maximum: usize,
) -> Result<WorkspaceFilePreview, HostError> {
    Err(HostError::Query(
        "safe workspace preview is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct IgnoreRules(Option<Arc<IgnoreRuleNode>>);

#[cfg(unix)]
struct IgnoreRuleNode {
    matcher: Gitignore,
    parent: Option<Arc<IgnoreRuleNode>>,
}

#[cfg(unix)]
impl IgnoreRules {
    fn with_matcher(&self, matcher: Gitignore) -> Self {
        if matcher.is_empty() {
            return self.clone();
        }
        Self(Some(Arc::new(IgnoreRuleNode {
            matcher,
            parent: self.0.clone(),
        })))
    }

    fn is_ignored(&self, relative: &Path, is_directory: bool) -> bool {
        let mut current = self.0.as_deref();
        while let Some(node) = current {
            let matched = node.matcher.matched(relative, is_directory);
            if matched.is_ignore() {
                return true;
            }
            if matched.is_whitelist() {
                return false;
            }
            current = node.parent.as_deref();
        }
        false
    }
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct WorkspaceIgnoreRules {
    git: IgnoreRules,
    tool: IgnoreRules,
}

#[cfg(unix)]
impl WorkspaceIgnoreRules {
    fn with_directory(
        &self,
        directory: &OwnedFd,
        relative_directory: &Path,
        root: bool,
        workspace: &Path,
    ) -> Result<Self, ()> {
        let matcher_root = if relative_directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative_directory
        };
        let mut git_builder = GitignoreBuilder::new(matcher_root);
        let mut git_patterns = 0_usize;
        if root {
            add_git_info_exclude(&mut git_builder, directory, workspace, &mut git_patterns)?;
        }
        add_ignore_file(
            &mut git_builder,
            directory,
            Path::new(".gitignore"),
            &mut git_patterns,
        )?;
        let git_matcher = git_builder.build().map_err(|_| ())?;

        let mut tool_builder = GitignoreBuilder::new(matcher_root);
        let mut tool_patterns = 0_usize;
        add_ignore_file(
            &mut tool_builder,
            directory,
            Path::new(".ignore"),
            &mut tool_patterns,
        )?;
        let tool_matcher = tool_builder.build().map_err(|_| ())?;
        Ok(Self {
            git: self.git.with_matcher(git_matcher),
            tool: self.tool.with_matcher(tool_matcher),
        })
    }

    fn is_ignored(&self, relative: &Path, is_directory: bool) -> bool {
        // Tool-specific whitelists must never revive paths excluded by Git.
        self.git.is_ignored(relative, is_directory) || self.tool.is_ignored(relative, is_directory)
    }
}

#[cfg(unix)]
enum IgnoreFile {
    Missing,
    Content(String),
    Unsafe,
}

#[cfg(unix)]
enum GitInfoExclude {
    Missing,
    Strict(String),
    External(String),
}

#[cfg(unix)]
fn read_bounded_ignore_file(directory: &OwnedFd, relative: &Path) -> IgnoreFile {
    let components = relative.components().collect::<Vec<_>>();
    let Ok(mut parent) = rustix::fs::openat(
        directory,
        ".",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
        return IgnoreFile::Unsafe;
    };
    let mut file = None;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return IgnoreFile::Unsafe;
        };
        let final_component = index.saturating_add(1) == components.len();
        let mut flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC;
        if !final_component {
            flags |= rustix::fs::OFlags::DIRECTORY;
        }
        match rustix::fs::openat(&parent, *name, flags, rustix::fs::Mode::empty()) {
            Ok(opened) if final_component => file = Some(opened),
            Ok(opened) => parent = opened,
            Err(error) if error == rustix::io::Errno::NOENT => return IgnoreFile::Missing,
            Err(_) => return IgnoreFile::Unsafe,
        }
    }
    let Some(file) = file else {
        return IgnoreFile::Unsafe;
    };
    let Ok(stat) = rustix::fs::fstat(&file) else {
        return IgnoreFile::Unsafe;
    };
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_size < 0
        || usize::try_from(stat.st_size).map_or(true, |size| size > MAX_IGNORE_FILE_BYTES)
    {
        return IgnoreFile::Unsafe;
    }
    let mut bytes = Vec::new();
    let Ok(maximum) = u64::try_from(MAX_IGNORE_FILE_BYTES) else {
        return IgnoreFile::Unsafe;
    };
    if fs::File::from(file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return IgnoreFile::Unsafe;
    }
    if bytes.len() > MAX_IGNORE_FILE_BYTES {
        return IgnoreFile::Unsafe;
    }
    String::from_utf8(bytes).map_or(IgnoreFile::Unsafe, IgnoreFile::Content)
}

#[cfg(unix)]
pub(super) fn bounded_gitdir_path(content: &str, prefix: Option<&str>) -> Option<PathBuf> {
    if content.len() > MAX_GITDIR_POINTER_BYTES {
        return None;
    }
    let content = content.strip_suffix('\n').unwrap_or(content);
    let content = content.strip_suffix('\r').unwrap_or(content);
    let value = prefix.map_or(Some(content), |prefix| content.strip_prefix(prefix))?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    Some(PathBuf::from(value))
}

#[cfg(unix)]
pub(super) fn open_linked_git_directory(base: &Path, path: &Path) -> Option<(PathBuf, OwnedFd)> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let canonical = fs::canonicalize(path).ok()?;
    let directory = open_workspace_directory(&canonical).ok()?;
    Some((canonical, directory))
}

#[cfg(unix)]
pub(super) fn linked_git_info_exclude(workspace: &Path, git_pointer: &str) -> Option<String> {
    let gitdir = bounded_gitdir_path(git_pointer, Some("gitdir: "))?;
    let (gitdir_path, gitdir) = open_linked_git_directory(workspace, &gitdir)?;
    let common = match read_bounded_ignore_file(&gitdir, Path::new("commondir")) {
        IgnoreFile::Missing => gitdir,
        IgnoreFile::Content(content) => {
            let common = bounded_gitdir_path(&content, None)?;
            open_linked_git_directory(&gitdir_path, &common)?.1
        }
        IgnoreFile::Unsafe => return None,
    };
    match read_bounded_ignore_file(&common, Path::new("info/exclude")) {
        IgnoreFile::Content(content) => Some(content),
        IgnoreFile::Missing | IgnoreFile::Unsafe => None,
    }
}

#[cfg(unix)]
fn read_git_info_exclude(directory: &OwnedFd, workspace: &Path) -> Result<GitInfoExclude, ()> {
    let metadata =
        match rustix::fs::statat(directory, ".git", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(GitInfoExclude::Missing),
            Err(_) => return Err(()),
        };
    let kind = rustix::fs::FileType::from_raw_mode(metadata.st_mode);
    if kind.is_dir() {
        return match read_bounded_ignore_file(directory, Path::new(".git/info/exclude")) {
            IgnoreFile::Missing => Ok(GitInfoExclude::Missing),
            IgnoreFile::Content(content) => Ok(GitInfoExclude::Strict(content)),
            IgnoreFile::Unsafe => Err(()),
        };
    }
    if !kind.is_file() {
        return Ok(GitInfoExclude::Missing);
    }
    let IgnoreFile::Content(pointer) = read_bounded_ignore_file(directory, Path::new(".git"))
    else {
        // An unavailable external gitdir must not make ordinary workspace
        // files disappear from the picker.
        return Ok(GitInfoExclude::Missing);
    };
    Ok(linked_git_info_exclude(workspace, &pointer)
        .map_or(GitInfoExclude::Missing, GitInfoExclude::External))
}

#[cfg(unix)]
pub(super) fn valid_ignore_patterns(content: &str) -> bool {
    let mut builder = GitignoreBuilder::new(".");
    let mut patterns = 0;
    add_ignore_patterns(&mut builder, content, &mut patterns).is_ok() && builder.build().is_ok()
}

#[cfg(unix)]
pub(super) fn add_git_info_exclude(
    builder: &mut GitignoreBuilder,
    directory: &OwnedFd,
    workspace: &Path,
    patterns: &mut usize,
) -> Result<(), ()> {
    match read_git_info_exclude(directory, workspace)? {
        GitInfoExclude::Strict(content) => add_ignore_patterns(builder, &content, patterns),
        GitInfoExclude::External(content) if valid_ignore_patterns(&content) => {
            add_ignore_patterns(builder, &content, patterns)
        }
        GitInfoExclude::Missing | GitInfoExclude::External(_) => Ok(()),
    }
}

#[cfg(unix)]
pub(super) fn add_ignore_file(
    builder: &mut GitignoreBuilder,
    directory: &OwnedFd,
    relative: &Path,
    patterns: &mut usize,
) -> Result<(), ()> {
    match read_bounded_ignore_file(directory, relative) {
        IgnoreFile::Missing => Ok(()),
        IgnoreFile::Content(content) => add_ignore_patterns(builder, &content, patterns),
        IgnoreFile::Unsafe => Err(()),
    }
}

#[cfg(unix)]
pub(super) fn add_ignore_patterns(
    builder: &mut GitignoreBuilder,
    content: &str,
    patterns: &mut usize,
) -> Result<(), ()> {
    for (index, line) in content.lines().enumerate() {
        if *patterns >= MAX_IGNORE_PATTERNS_PER_DIRECTORY {
            return Err(());
        }
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if line.len() > MAX_IGNORE_PATTERN_BYTES {
            return Err(());
        }
        builder.add_line(None, line).map_err(|_| ())?;
        *patterns = patterns.saturating_add(1);
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn fuzzy_path_matches(path: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut path = path.chars();
    query.chars().all(|needle| {
        path.by_ref()
            .any(|candidate| candidate.eq_ignore_ascii_case(&needle))
    })
}

#[cfg(unix)]
pub(super) fn search_workspace(
    workspace: &Path,
    query: &str,
    limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Ok((Vec::new(), true));
    }
    let started = Instant::now();
    let root = open_workspace_directory(workspace)?;
    let mut pending = vec![(root, PathBuf::new(), WorkspaceIgnoreRules::default(), true)];
    let mut matches: BTreeMap<String, bool> = BTreeMap::new();
    let mut visited = 0_usize;
    let mut truncated = false;
    while let Some((directory, relative_directory, parent_rules, is_root)) = pending.pop() {
        if started.elapsed() >= QUERY_DEADLINE || visited >= MAX_SEARCH_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(rules) =
            parent_rules.with_directory(&directory, &relative_directory, is_root, workspace)
        else {
            // An unsafe ignore control file makes this entire subtree
            // indeterminate. Never expose entries by silently ignoring it.
            truncated = true;
            if !relative_directory.as_os_str().is_empty() {
                let subtree = relative_directory.to_string_lossy();
                matches.retain(|path, _| {
                    path.as_str() != subtree.as_ref()
                        && !path
                            .strip_prefix(subtree.as_ref())
                            .is_some_and(|suffix| suffix.starts_with('/'))
                });
            }
            continue;
        };
        let entries = rustix::fs::Dir::read_from(&directory)
            .map_err(|_| HostError::Query("workspace directory could not be read".to_owned()))?;
        for entry in entries {
            if started.elapsed() >= QUERY_DEADLINE || visited >= MAX_SEARCH_ENTRIES {
                truncated = true;
                break;
            }
            let entry = entry
                .map_err(|_| HostError::Query("workspace directory read failed".to_owned()))?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b".." | b".git") {
                continue;
            }
            let name = std::ffi::OsStr::from_bytes(name.to_bytes());
            let Some(name_text) = name.to_str() else {
                continue;
            };
            visited = visited.saturating_add(1);
            let Ok(child) = rustix::fs::openat(
                &directory,
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            ) else {
                continue;
            };
            let Ok(stat) = rustix::fs::fstat(&child) else {
                continue;
            };
            let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            if !file_type.is_file() && !file_type.is_dir() {
                continue;
            }
            let relative = relative_directory.join(name_text);
            if rules.is_ignored(&relative, file_type.is_dir()) {
                continue;
            }
            let rendered = relative.to_string_lossy().into_owned();
            if fuzzy_path_matches(&rendered, query) {
                matches.insert(rendered, file_type.is_dir());
                if matches.len() > limit {
                    let _ = matches.pop_last();
                    truncated = true;
                }
            }
            if file_type.is_dir() {
                pending.push((child, relative, rules.clone(), false));
            }
        }
        if started.elapsed() >= QUERY_DEADLINE || visited >= MAX_SEARCH_ENTRIES {
            break;
        }
    }
    let matches = matches
        .into_iter()
        .map(|(path, is_directory)| WorkspaceFileMatch { path, is_directory })
        .collect();
    Ok((matches, truncated))
}

#[cfg(not(unix))]
pub(super) fn search_workspace(
    _workspace: &Path,
    _query: &str,
    _limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    Err(HostError::Query(
        "safe workspace search is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
pub(super) fn open_workspace_directory(workspace: &Path) -> Result<OwnedFd, HostError> {
    rustix::fs::open(
        workspace,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| HostError::Query("workspace directory could not be opened safely".to_owned()))
}

#[cfg(unix)]
pub(super) fn open_relative_regular_file(
    root: &OwnedFd,
    relative: &Path,
) -> Result<OwnedFd, HostError> {
    let components = relative.components().collect::<Vec<_>>();
    let mut directory = rustix::fs::openat(
        root,
        ".",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| HostError::Query("workspace directory could not be opened safely".to_owned()))?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(HostError::Query("workspace path is invalid".to_owned()));
        };
        let final_component = index.saturating_add(1) == components.len();
        let mut flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC;
        if !final_component {
            flags |= rustix::fs::OFlags::DIRECTORY;
        }
        let opened = rustix::fs::openat(&directory, *name, flags, rustix::fs::Mode::empty())
            .map_err(|_| {
                HostError::Query("workspace path could not be opened safely".to_owned())
            })?;
        if final_component {
            return Ok(opened);
        }
        directory = opened;
    }
    Err(HostError::Query("workspace path is invalid".to_owned()))
}
