use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::bash::audited_system_git;
use crate::registry::{CancellationToken, ToolError};

use super::{DIAGNOSTIC_LIMIT, MAX_GIT_OUTPUT_BYTES, canonical_directory};

pub(super) async fn append_untracked_patches(
    root: &Path,
    mut patch: Vec<u8>,
    paths: Vec<PathBuf>,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, ToolError> {
    for path in paths {
        let added = run_git(
            root,
            [
                OsString::from("diff"),
                OsString::from("--no-index"),
                OsString::from("--binary"),
                OsString::from("--full-index"),
                OsString::from("--no-ext-diff"),
                OsString::from("--no-textconv"),
                OsString::from("--"),
                OsString::from("/dev/null"),
                path.as_os_str().to_os_string(),
            ],
            None,
            cancellation,
        )
        .await?;
        if added.status.code() != Some(1) || added.stdout.is_empty() {
            return Err(ToolError::Command(format!(
                "capture isolated untracked file failed: {}",
                bounded_diagnostic(&added)
            )));
        }
        patch.extend_from_slice(&added.stdout);
        if patch.len() > limit {
            return Err(ToolError::SizeLimit { limit });
        }
    }
    Ok(patch)
}

pub(super) async fn git_common_directory(
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<PathBuf, ToolError> {
    let output = run_git(
        root,
        [
            OsString::from("rev-parse"),
            OsString::from("--git-common-dir"),
        ],
        None,
        cancellation,
    )
    .await?;
    require_success("discover git common directory", &output)?;
    let path = path_from_stdout(&output.stdout, "git common directory")?;
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.canonicalize().map_err(|source| ToolError::Io {
        operation: "canonicalize git common directory",
        path,
        source,
    })
}

pub(super) async fn git_index_path(
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<PathBuf, ToolError> {
    let output = run_git(
        root,
        [
            OsString::from("rev-parse"),
            OsString::from("--git-path"),
            OsString::from("index"),
        ],
        None,
        cancellation,
    )
    .await?;
    require_success("discover git index", &output)?;
    let path = path_from_stdout(&output.stdout, "git index")?;
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.canonicalize().map_err(|source| ToolError::Io {
        operation: "canonicalize git index",
        path,
        source,
    })
}

pub(super) async fn validate_repository_root(
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<(), ToolError> {
    let canonical = canonical_directory(root, "apply repository root")?;
    if canonical != root {
        return Err(ToolError::Command(
            "apply workspace root changed after context creation".to_owned(),
        ));
    }
    let output = run_git(
        root,
        [
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ],
        None,
        cancellation,
    )
    .await?;
    require_success("verify apply repository", &output)?;
    let reported = canonical_directory(
        &path_from_stdout(&output.stdout, "repository root")?,
        "repository root",
    )?;
    if reported != root {
        return Err(ToolError::Command(
            "apply workspace is not the exact repository root".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct GitOutput {
    pub(super) status: std::process::ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

pub(super) async fn run_git<I>(
    cwd: &Path,
    args: I,
    stdin: Option<&[u8]>,
    cancellation: &CancellationToken,
) -> Result<GitOutput, ToolError>
where
    I: IntoIterator<Item = OsString>,
{
    run_git_with_paths(cwd, args, stdin, cancellation, None, None).await
}

pub(super) async fn run_git_with_paths<I>(
    cwd: &Path,
    args: I,
    stdin: Option<&[u8]>,
    cancellation: &CancellationToken,
    index_file: Option<&Path>,
    work_tree: Option<&Path>,
) -> Result<GitOutput, ToolError>
where
    I: IntoIterator<Item = OsString>,
{
    let configured = run_git_raw_with_paths(
        cwd,
        [
            OsString::from("config"),
            OsString::from("--local"),
            OsString::from("--name-only"),
            OsString::from("--get-regexp"),
            OsString::from(r"^filter\..*\.(clean|smudge|process|required)$"),
        ],
        None,
        cancellation,
        index_file,
        work_tree,
    )
    .await?;
    if !configured.status.success() && configured.status.code() != Some(1) {
        return Err(ToolError::Command(format!(
            "inspect repository filter configuration failed: {}",
            bounded_diagnostic(&configured)
        )));
    }
    let mut drivers = BTreeSet::new();
    for key in String::from_utf8_lossy(&configured.stdout).lines() {
        let Some(body) = key.strip_prefix("filter.") else {
            continue;
        };
        let Some((driver, _property)) = body.rsplit_once('.') else {
            continue;
        };
        if !driver.is_empty() {
            drivers.insert(driver.to_owned());
        }
    }
    let mut safe_args = Vec::new();
    for driver in drivers {
        for (property, value) in [
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            safe_args.push(OsString::from("-c"));
            safe_args.push(OsString::from(format!(
                "filter.{driver}.{property}={value}"
            )));
        }
    }
    safe_args.extend(args);
    run_git_raw_with_paths(cwd, safe_args, stdin, cancellation, index_file, work_tree).await
}

pub(super) async fn run_git_raw_with_paths<I>(
    cwd: &Path,
    args: I,
    stdin: Option<&[u8]>,
    cancellation: &CancellationToken,
    index_file: Option<&Path>,
    work_tree: Option<&Path>,
) -> Result<GitOutput, ToolError>
where
    I: IntoIterator<Item = OsString>,
{
    cancellation.check()?;
    let git = audited_system_git().ok_or_else(|| {
        ToolError::Command("no audited root-owned system git executable is available".to_owned())
    })?;
    let mut command = configured_git_command(git, cwd, args, index_file, work_tree);
    let mut child = command
        .spawn()
        .map_err(|source| ToolError::Command(format!("could not spawn audited git: {source}")))?;
    let child_id = child.id();
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| ToolError::Command("git stdin pipe was unavailable".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Command("git stdout pipe was unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Command("git stderr pipe was unavailable".to_owned()))?;
    let stdout_task = tokio::spawn(read_bounded(stdout, MAX_GIT_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(read_bounded(stderr, DIAGNOSTIC_LIMIT));
    let write = async {
        if let Some(bytes) = stdin {
            child_stdin.write_all(bytes).await.map_err(|source| {
                ToolError::Command(format!("could not write git input: {source}"))
            })?;
        }
        child_stdin
            .shutdown()
            .await
            .map_err(|source| ToolError::Command(format!("could not close git input: {source}")))
    };
    let write_result = tokio::select! {
        result = write => result,
        () = cancellation.cancelled() => Err(ToolError::Cancelled),
    };
    drop(child_stdin);
    if let Err(error) = write_result {
        terminate_process_group(child_id);
        let _ = child.start_kill();
        let _ = child.wait().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        return Err(error);
    }
    let status = tokio::select! {
        status = child.wait() => status.map_err(|source| ToolError::Command(format!("could not wait for git: {source}")))?,
        () = cancellation.cancelled() => {
            terminate_process_group(child_id);
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ToolError::Cancelled);
        }
    };
    let (stdout, stdout_truncated) = stdout_task
        .await
        .map_err(|source| ToolError::Output(source.to_string()))??;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .map_err(|source| ToolError::Output(source.to_string()))??;
    if stdout_truncated || stderr_truncated {
        return Err(ToolError::SizeLimit {
            limit: if stdout_truncated {
                MAX_GIT_OUTPUT_BYTES
            } else {
                DIAGNOSTIC_LIMIT
            },
        });
    }
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

pub(super) fn configured_git_command<I>(
    git: &Path,
    cwd: &Path,
    args: I,
    index_file: Option<&Path>,
    work_tree: Option<&Path>,
) -> Command
where
    I: IntoIterator<Item = OsString>,
{
    let mut command = Command::new(git);
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("diff.external=")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", OsStr::new("/dev/null"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(index_file) = index_file {
        command.env("GIT_INDEX_FILE", index_file);
    }
    if let Some(work_tree) = work_tree {
        command.env("GIT_WORK_TREE", work_tree);
    }
    #[cfg(unix)]
    command.process_group(0);
    command
}

pub(super) async fn read_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<(Vec<u8>, bool), ToolError> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|source| ToolError::Output(source.to_string()))?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retain = remaining.min(read);
        bytes.extend_from_slice(&chunk[..retain]);
        truncated |= retain < read;
    }
    Ok((bytes, truncated))
}

#[cfg(unix)]
pub(super) fn terminate_process_group(child_id: Option<u32>) {
    let Some(raw) = child_id.and_then(|id| i32::try_from(id).ok()) else {
        return;
    };
    if let Some(pid) = rustix::process::Pid::from_raw(raw) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
pub(super) fn terminate_process_group(_child_id: Option<u32>) {}

pub(super) fn path_from_stdout(bytes: &[u8], label: &str) -> Result<PathBuf, ToolError> {
    Ok(PathBuf::from(text_stdout(bytes, label)?))
}

pub(super) fn text_stdout(bytes: &[u8], label: &str) -> Result<String, ToolError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ToolError::Output(format!("git emitted non-UTF-8 {label}")))?
        .trim();
    if text.is_empty() {
        return Err(ToolError::Output(format!("git emitted empty {label}")));
    }
    Ok(text.to_owned())
}

pub(super) fn require_success(operation: &str, output: &GitOutput) -> Result<(), ToolError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(ToolError::Command(format!(
            "{operation} failed: {}",
            bounded_diagnostic(output)
        )))
    }
}

pub(super) fn bounded_diagnostic(output: &GitOutput) -> String {
    let bytes = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    String::from_utf8_lossy(&bytes[..bytes.len().min(DIAGNOSTIC_LIMIT)])
        .trim()
        .to_owned()
}

pub(super) fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}
