use super::*;

pub(super) fn read_workspace_status(
    workspace: &Path,
    workspace_name: String,
) -> Result<WorkspaceStatus, HostError> {
    let branch = read_git_branch(workspace)?;
    let (changed_paths, truncated) = read_git_changed_paths(workspace);
    Ok(WorkspaceStatus {
        workspace_name,
        branch,
        changed_paths,
        truncated,
    })
}

#[cfg(unix)]
pub(super) struct GitCommandOutput {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) overflow: bool,
}

#[cfg(unix)]
pub(super) fn resolve_git_executable_from_candidates<'a>(
    candidates: impl IntoIterator<Item = &'a Path>,
) -> Option<PathBuf> {
    candidates.into_iter().find_map(|candidate| {
        let canonical = fs::canonicalize(candidate).ok()?;
        let metadata = fs::metadata(&canonical).ok()?;
        // Automatic status queries run without a user gesture. Only accept a
        // root-owned, executable system binary that cannot be replaced by an
        // unprivileged user. In particular, never execute a `git` selected
        // from the caller's user-writable PATH.
        (metadata.is_file()
            && metadata.uid() == 0
            && metadata.mode() & 0o022 == 0
            && metadata.permissions().mode() & 0o111 != 0)
            .then_some(canonical)
    })
}

#[cfg(unix)]
pub(super) fn resolve_git_executable_for_caller_path(
    _caller_path: Option<&OsStr>,
) -> Option<PathBuf> {
    resolve_git_executable_from_candidates([Path::new("/usr/bin/git"), Path::new("/bin/git")])
}

#[cfg(unix)]
pub(super) fn resolve_git_executable(_workspace: &Path) -> Option<PathBuf> {
    let caller_path = std::env::var_os("PATH");
    resolve_git_executable_for_caller_path(caller_path.as_deref())
}

#[cfg(unix)]
pub(super) fn kill_git_process_group(child: &mut std::process::Child) {
    if let Ok(raw_pid) = i32::try_from(child.id())
        && let Some(pid) = rustix::process::Pid::from_raw(raw_pid)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

// One caller owns process and pipe; drain deadlines never wait for a reader thread.
#[cfg(unix)]
fn capture_git_output(
    child: &mut std::process::Child,
    maximum: usize,
    deadline: Duration,
) -> Option<(ExitStatus, Vec<u8>, bool)> {
    let mut stdout = child.stdout.take()?;
    let flags = rustix::fs::fcntl_getfl(&stdout).ok()?;
    rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK).ok()?;
    let started = Instant::now();
    let mut status = None;
    let mut exited_at = None;
    let mut killed_at = None;
    let mut eof = false;
    let mut captured = Vec::new();
    let mut overflow = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = if eof {
            0
        } else {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    0
                }
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => 0,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => 0,
                Err(_) => return None,
            }
        };
        let retained = maximum.saturating_sub(captured.len()).min(read);
        captured.extend_from_slice(&buffer[..retained]);
        overflow |= retained < read;
        if status.is_none() {
            if let Some(exited) = child.try_wait().ok()? {
                status = Some(exited);
                exited_at = Some(Instant::now());
            } else if started.elapsed() >= deadline {
                return None;
            }
        }
        if let Some(status) = status {
            if eof {
                return Some((status, captured, overflow));
            }
            if killed_at.is_some_and(|at: Instant| at.elapsed() >= GIT_READER_DEADLINE) {
                return None;
            }
            if killed_at.is_none()
                && exited_at.is_some_and(|at| at.elapsed() >= GIT_READER_DEADLINE)
            {
                kill_git_process_group(child);
                killed_at = Some(Instant::now());
            }
        }
        if read == 0 {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(unix)]
pub(super) fn run_bounded_git(
    git: &Path,
    workspace: &Path,
    arguments: &[&OsStr],
    maximum: usize,
    deadline: Duration,
) -> Option<GitCommandOutput> {
    if !git.is_absolute() {
        return None;
    }
    let root = open_workspace_directory(workspace).ok()?;
    let root_stat = rustix::fs::fstat(&root).ok()?;
    let mut command = Command::new(git);
    command
        .current_dir(workspace)
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.untrackedCache=false"])
        .args(["-c", "submodule.recurse=false"])
        .args(["-c", "diff.external="])
        .args(arguments)
        // Git itself is absolute and trusted. Restrict any helper lookup to
        // system locations as defense in depth against a hostile caller PATH.
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_PAGER", "cat")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().ok()?;
    let output = capture_git_output(&mut child, maximum, deadline);
    // Closing this scope closes the nonblocking pipe even if a descendant kept
    // it open after leaving the process group; no detached reader survives us.
    kill_git_process_group(&mut child);
    let (status, stdout, overflow) = output?;
    let identity_unchanged = open_workspace_directory(workspace)
        .and_then(|current| {
            rustix::fs::fstat(&current)
                .map_err(|_| HostError::Query("workspace identity is unavailable".to_owned()))
        })
        .is_ok_and(|current| {
            current.st_dev == root_stat.st_dev && current.st_ino == root_stat.st_ino
        });
    if !identity_unchanged {
        return None;
    }
    Some(GitCommandOutput {
        status,
        stdout,
        overflow,
    })
}

#[cfg(unix)]
pub(super) fn read_git_changed_paths(workspace: &Path) -> (Vec<String>, bool) {
    let Ok(root) = open_workspace_directory(workspace) else {
        return (Vec::new(), true);
    };
    match rustix::fs::statat(&root, ".git", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Err(error) if error == rustix::io::Errno::NOENT => return (Vec::new(), false),
        Ok(stat) => {
            let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            if !kind.is_file() && !kind.is_dir() {
                return (Vec::new(), true);
            }
        }
        Err(_) => return (Vec::new(), true),
    }
    let Some(git) = resolve_git_executable(workspace) else {
        return (Vec::new(), true);
    };
    let arguments = [
        OsStr::new("status"),
        OsStr::new("--porcelain=v1"),
        OsStr::new("-z"),
        OsStr::new("--untracked-files=all"),
        OsStr::new("--ignored=no"),
    ];
    let Some(output) = run_bounded_git(
        &git,
        workspace,
        &arguments,
        MAX_GIT_STATUS_BYTES,
        GIT_STATUS_DEADLINE,
    ) else {
        return (Vec::new(), true);
    };
    if !output.status.success() {
        return (Vec::new(), true);
    }
    parse_git_status(&output.stdout, output.overflow)
}

#[cfg(not(unix))]
pub(super) fn read_git_changed_paths(_workspace: &Path) -> (Vec<String>, bool) {
    (Vec::new(), false)
}

#[cfg(unix)]
pub(super) fn parse_git_status(bytes: &[u8], mut truncated: bool) -> (Vec<String>, bool) {
    let complete_bytes = match bytes.iter().rposition(|byte| *byte == 0) {
        Some(last_nul) => {
            truncated |= last_nul.saturating_add(1) != bytes.len();
            &bytes[..=last_nul]
        }
        None => {
            return (Vec::new(), !bytes.is_empty() || truncated);
        }
    };
    let mut paths = BTreeSet::new();
    let mut records = complete_bytes.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            truncated = true;
            continue;
        }
        let status = &record[..2];
        let path = &record[3..];
        if status.iter().any(|code| matches!(*code, b'R' | b'C')) {
            // Porcelain v1 -z follows a rename/copy destination with its
            // source path. Only the destination is actionable in the UI.
            if records.next().is_none() {
                truncated = true;
            }
        }
        let Ok(path) = std::str::from_utf8(path) else {
            truncated = true;
            continue;
        };
        let Ok(path) = safe_relative_path(path) else {
            truncated = true;
            continue;
        };
        if path
            .components()
            .any(|component| component.as_os_str() == ".git")
        {
            continue;
        }
        let Some(path) = path.to_str() else {
            truncated = true;
            continue;
        };
        paths.insert(path.to_owned());
        if paths.len() >= MAX_CHANGED_PATHS {
            truncated |= records.any(|record| !record.is_empty());
            break;
        }
    }
    (paths.into_iter().collect(), truncated)
}

#[cfg(unix)]
// Keeping classification, identity-bound Git calls, and fail-closed branches
// together makes the security order auditable.
#[allow(clippy::too_many_lines)]
pub(super) fn read_workspace_diff(
    workspace: &Path,
    relative: &Path,
    maximum: usize,
) -> Result<WorkspaceDiff, HostError> {
    let path = relative
        .to_str()
        .filter(|path| {
            !path.is_empty()
                && !path
                    .chars()
                    .any(|character| character.is_control() || character == '\0')
        })
        .ok_or_else(|| {
            HostError::Query("workspace diff path is not safely renderable".to_owned())
        })?;
    let root = open_workspace_directory(workspace)?;
    let git_marker = rustix::fs::statat(&root, ".git", rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| HostError::Query("workspace is not a readable Git repository".to_owned()))?;
    let marker_kind = rustix::fs::FileType::from_raw_mode(git_marker.st_mode);
    if !marker_kind.is_file() && !marker_kind.is_dir() {
        return Err(HostError::Query(
            "workspace Git metadata is unsafe".to_owned(),
        ));
    }
    let git = resolve_git_executable(workspace)
        .ok_or_else(|| HostError::Query("trusted Git executable is unavailable".to_owned()))?;
    let relative_os = relative.as_os_str();
    let tracked_arguments = [
        OsStr::new("ls-files"),
        OsStr::new("--error-unmatch"),
        OsStr::new("--"),
        relative_os,
    ];
    let tracked = run_bounded_git(&git, workspace, &tracked_arguments, 1, GIT_DIFF_DEADLINE)
        .ok_or_else(|| HostError::Query("Git path classification failed".to_owned()))?;
    if tracked.status.success() {
        let arguments = [
            OsStr::new("diff"),
            OsStr::new("--no-ext-diff"),
            OsStr::new("--no-textconv"),
            OsStr::new("--no-color"),
            OsStr::new("HEAD"),
            OsStr::new("--"),
            relative_os,
        ];
        let output = run_bounded_git(&git, workspace, &arguments, maximum, GIT_DIFF_DEADLINE)
            .ok_or_else(|| HostError::Query("Git diff failed".to_owned()))?;
        if !output.status.success() {
            return Err(HostError::Query("Git diff failed".to_owned()));
        }
        let binary = output
            .stdout
            .windows(12)
            .any(|window| window == b"Binary files")
            || output
                .stdout
                .windows(16)
                .any(|window| window == b"GIT binary patch");
        let (unified_diff, truncated, invalid_utf8) =
            if let Ok(diff) = String::from_utf8(output.stdout) {
                let (diff, truncated) = bounded_diff_text(diff, maximum, output.overflow);
                (diff, truncated, false)
            } else {
                let (diff, truncated) = bounded_diff_text(binary_diff(path), maximum, true);
                (diff, truncated, true)
            };
        return Ok(WorkspaceDiff {
            path: path.to_owned(),
            unified_diff,
            truncated,
            binary: binary || invalid_utf8,
        });
    }
    if tracked.status.code() != Some(1) {
        return Err(HostError::Query(
            "Git path classification failed".to_owned(),
        ));
    }

    let ignored_arguments = [
        OsStr::new("check-ignore"),
        OsStr::new("--quiet"),
        OsStr::new("--"),
        relative_os,
    ];
    let ignored = run_bounded_git(&git, workspace, &ignored_arguments, 1, GIT_DIFF_DEADLINE)
        .ok_or_else(|| HostError::Query("Git ignore classification failed".to_owned()))?;
    if ignored.status.success() {
        return Err(HostError::Query(
            "workspace diff refuses Git-ignored files".to_owned(),
        ));
    }
    if ignored.status.code() != Some(1) {
        return Err(HostError::Query(
            "Git ignore classification failed".to_owned(),
        ));
    }

    let file = open_relative_regular_file(&root, relative)?;
    let stat = rustix::fs::fstat(&file)
        .map_err(|_| HostError::Query("workspace file metadata is unavailable".to_owned()))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(HostError::Query(
            "workspace diff accepts regular files only".to_owned(),
        ));
    }
    let total = usize::try_from(stat.st_size).unwrap_or(usize::MAX);
    let mut bytes = Vec::new();
    fs::File::from(file)
        .take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| HostError::Query("workspace file could not be read".to_owned()))?;
    let source_truncated = total > maximum || bytes.len() > maximum;
    bytes.truncate(maximum);
    let executable = stat.st_mode & 0o111 != 0;
    if bytes.contains(&0) {
        let (unified_diff, truncated) =
            bounded_diff_text(binary_diff(path), maximum, source_truncated);
        return Ok(WorkspaceDiff {
            path: path.to_owned(),
            unified_diff,
            truncated,
            binary: true,
        });
    }
    let Ok(content) = String::from_utf8(bytes) else {
        let (unified_diff, truncated) = bounded_diff_text(binary_diff(path), maximum, true);
        return Ok(WorkspaceDiff {
            path: path.to_owned(),
            unified_diff,
            truncated,
            binary: true,
        });
    };
    let rendered = render_untracked_diff(path, &content, executable);
    let (unified_diff, truncated) = bounded_diff_text(rendered, maximum, source_truncated);
    Ok(WorkspaceDiff {
        path: path.to_owned(),
        unified_diff,
        truncated,
        binary: false,
    })
}

#[cfg(not(unix))]
pub(super) fn read_workspace_diff(
    _workspace: &Path,
    _relative: &Path,
    _maximum: usize,
) -> Result<WorkspaceDiff, HostError> {
    Err(HostError::Query(
        "safe workspace diff is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
pub(super) fn render_untracked_diff(path: &str, content: &str, executable: bool) -> String {
    let line_count = content.lines().count().max(1);
    let mut diff = format!(
        "diff --git a/{path} b/{path}\nnew file mode {}\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{line_count} @@\n",
        if executable { "100755" } else { "100644" }
    );
    for line in content.split_inclusive('\n') {
        diff.push('+');
        diff.push_str(line);
    }
    if !content.is_empty() && !content.ends_with('\n') {
        diff.push_str("\n\\ No newline at end of file\n");
    }
    if content.is_empty() {
        diff.push_str("+\n");
    }
    diff
}

#[cfg(unix)]
pub(super) fn binary_diff(path: &str) -> String {
    format!("diff --git a/{path} b/{path}\nBinary files /dev/null and b/{path} differ\n")
}

#[cfg(unix)]
pub(super) fn bounded_diff_text(
    mut text: String,
    maximum: usize,
    mut truncated: bool,
) -> (String, bool) {
    if text.len() <= maximum {
        return (text, truncated);
    }
    truncated = true;
    let mut end = maximum;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end = text[..end].rfind('\n').map_or(0, |newline| newline + 1);
    text.truncate(end);
    (text, truncated)
}

#[cfg(unix)]
pub(super) fn read_git_branch(workspace: &Path) -> Result<Option<String>, HostError> {
    open_workspace_directory(workspace)?;
    let Some(git) = resolve_git_executable(workspace) else {
        return Ok(None);
    };
    let symbolic = [
        OsStr::new("symbolic-ref"),
        OsStr::new("--quiet"),
        OsStr::new("--short"),
        OsStr::new("HEAD"),
    ];
    if let Some(output) = run_bounded_git(&git, workspace, &symbolic, 512, GIT_STATUS_DEADLINE)
        && output.status.success()
        && !output.overflow
        && let Some(branch) = safe_git_label(&output.stdout)
    {
        return Ok(Some(branch));
    }
    let detached = [
        OsStr::new("rev-parse"),
        OsStr::new("--short=12"),
        OsStr::new("HEAD"),
    ];
    if let Some(output) = run_bounded_git(&git, workspace, &detached, 64, GIT_STATUS_DEADLINE)
        && output.status.success()
        && !output.overflow
        && let Some(revision) = safe_git_label(&output.stdout)
    {
        return Ok(Some(format!("detached@{revision}")));
    }
    Ok(None)
}

#[cfg(unix)]
pub(super) fn safe_git_label(bytes: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(bytes).ok()?.trim();
    if value.is_empty()
        || value.len() > 256
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(not(unix))]
pub(super) fn read_git_branch(_workspace: &Path) -> Result<Option<String>, HostError> {
    Ok(None)
}
