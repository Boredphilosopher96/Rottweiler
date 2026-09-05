use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{Duration, sleep};

use crate::registry::ToolError;

#[cfg(unix)]
pub(super) fn terminate_process_group(child_id: Option<u32>) {
    let Some(raw_pid) = child_id.and_then(|id| i32::try_from(id).ok()) else {
        return;
    };
    if let Some(pid) = rustix::process::Pid::from_raw(raw_pid) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

/// Terminates the process group and waits until no member can execute effects.
///
/// # Errors
/// Returns an error when the process group cannot be identified or inspected.
/// Remains pending when a live member cannot be stopped.
#[cfg(unix)]
pub async fn terminate_and_wait_process_group(child_id: Option<u32>) -> Result<(), ToolError> {
    let raw_pid = child_id
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| ToolError::Command("command process group id was unavailable".to_owned()))?;
    let _ = rustix::process::kill_process_group(raw_pid, rustix::process::Signal::KILL);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match rustix::process::test_kill_process_group(raw_pid) {
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            Ok(()) => {}
            Err(error) => {
                if macos_terminal_group_probe(error, raw_pid.as_raw_nonzero().get()).await {
                    return Ok(());
                }
                return Err(ToolError::Command(format!(
                    "could not verify command process-group exit: {error}"
                )));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            if linux_process_group_has_no_live_members(raw_pid.as_raw_nonzero().get()).await
                == Some(true)
            {
                return Ok(());
            }
            // Returning would allow the opaque-checkpoint post-scan to race a
            // surviving group member. Keep the operation pending and the
            // watchdog/lease armed: this is the fail-closed state.
            std::future::pending::<()>().await;
            unreachable!("pending process-group barrier cannot complete");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "linux")]
pub(super) async fn linux_process_group_has_no_live_members(process_group: i32) -> Option<bool> {
    tokio::task::spawn_blocking(move || {
        const ENTRY_CAP: usize = 32 * 1024;
        const STAT_CAP: u64 = 4 * 1024;
        const TOTAL_CAP: usize = 8 * 1024 * 1024;
        let entries = std::fs::read_dir("/proc").ok()?;
        let mut entry_count = 0_usize;
        let mut total_bytes = 0_usize;
        for entry in entries {
            entry_count = entry_count.checked_add(1)?;
            if entry_count > ENTRY_CAP {
                return None;
            }
            let entry = entry.ok()?;
            let file_name = entry.file_name();
            if !file_name.as_encoded_bytes().iter().all(u8::is_ascii_digit) {
                continue;
            }
            let mut stat = Vec::new();
            match std::fs::File::open(entry.path().join("stat")) {
                Ok(file) => {
                    file.take(STAT_CAP + 1).read_to_end(&mut stat).ok()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            }
            if stat.len() as u64 > STAT_CAP {
                return None;
            }
            total_bytes = total_bytes.checked_add(stat.len())?;
            if total_bytes > TOTAL_CAP {
                return None;
            }
            let (group, state) = parse_linux_process_stat(&stat)?;
            if group == process_group && state != b'Z' {
                return Some(false);
            }
        }
        Some(true)
    })
    .await
    .ok()
    .flatten()
}

#[cfg(target_os = "linux")]
pub(super) fn parse_linux_process_stat(stat: &[u8]) -> Option<(i32, u8)> {
    let stat = std::str::from_utf8(stat).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let mut fields = fields.split_ascii_whitespace();
    let state = fields.next()?.as_bytes();
    if state.len() != 1 {
        return None;
    }
    let _parent = fields.next()?.parse::<i32>().ok()?;
    let process_group = fields.next()?.parse::<i32>().ok()?;
    Some((process_group, state[0]))
}

#[cfg(not(target_os = "linux"))]
pub(super) fn linux_process_group_has_no_live_members(
    _process_group: i32,
) -> std::future::Ready<Option<bool>> {
    std::future::ready(None)
}

#[cfg(target_os = "macos")]
pub(super) async fn macos_terminal_group_probe(
    error: rustix::io::Errno,
    process_group: i32,
) -> bool {
    // Darwin may report EPERM for zombie-only groups, but EPERM can also mean
    // that a live member has different credentials. Never infer which case it
    // is from an earlier signal attempt; require an independent membership
    // snapshot that proves there are no executable members.
    error == rustix::io::Errno::PERM
        && matches!(
            macos_process_group_has_no_live_members(process_group).await,
            Some(true)
        )
}

#[cfg(target_os = "macos")]
pub(super) async fn macos_process_group_has_no_live_members(process_group: i32) -> Option<bool> {
    const OUTPUT_CAP: usize = 256 * 1024;
    // Invoke the trusted absolute system binary without a shell or caller
    // environment, and bound every resource before treating its output as a
    // security decision. `None` is an unknown result and remains fail-closed.
    let mut command = Command::new("/bin/ps");
    command
        .args(["-axo", "pgid=,stat="])
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let collected = tokio::time::timeout(Duration::from_secs(2), async {
        let mut output = Vec::new();
        stdout
            .take((OUTPUT_CAP + 1) as u64)
            .read_to_end(&mut output)
            .await
            .ok()?;
        if output.len() > OUTPUT_CAP {
            return None;
        }
        let status = child.wait().await.ok()?;
        status.success().then_some(output)
    })
    .await;
    let Ok(Some(output)) = collected else {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_millis(100), child.wait()).await;
        return None;
    };
    parse_macos_process_group_status(&output, process_group)
}

#[cfg(target_os = "macos")]
pub(super) fn parse_macos_process_group_status(output: &[u8], process_group: i32) -> Option<bool> {
    let output = std::str::from_utf8(output).ok()?;
    let mut saw_process = false;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_ascii_whitespace();
        let pgid = fields.next()?.parse::<i32>().ok()?;
        let status = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        saw_process = true;
        if pgid == process_group && !status.starts_with('Z') {
            return Some(false);
        }
    }
    saw_process.then_some(true)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn macos_terminal_group_probe(
    _error: rustix::io::Errno,
    _process_group: i32,
) -> std::future::Ready<bool> {
    std::future::ready(false)
}

#[cfg(not(unix))]
pub(super) fn terminate_process_group(_child_id: Option<u32>) {}

/// Reports that process group settlement is unavailable on this platform.
///
/// # Errors
/// Returns an unsupported-platform error.
#[cfg(not(unix))]
pub async fn terminate_and_wait_process_group(_child_id: Option<u32>) -> Result<(), ToolError> {
    Err(ToolError::Command(
        "process-group exit barriers are unavailable on this platform".to_owned(),
    ))
}
