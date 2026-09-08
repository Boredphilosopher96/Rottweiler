//! A pre-grant helper rejection remains owned through wait and finite diagnostics.
use super::{Child, PluginProcessError};
use rustix::process::{Pid, WaitId, WaitIdOptions};
use std::{io, process::ExitStatus, time::Duration};

const STDERR_BYTES: usize = 4096;

pub(super) fn retire(child: &mut Child, pid: u32, cause: &io::Error) -> PluginProcessError {
    let before_kill = observe_exit(pid);
    // No launch grant was sent: this helper cannot have started the effect.
    // Keep the child and caller's process admission until actual wait succeeds.
    let _ = child.start_kill();
    let runtime = tokio::runtime::Handle::current();
    let status = loop {
        match runtime.block_on(child.wait()) {
            Ok(status) => break status,
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let stderr = child.stderr.take().map_or_else(
        || "stderr unavailable".to_owned(),
        |stderr| read_stderr(&stderr),
    );
    PluginProcessError {
        // Do not trace raw helper output. The mandatory PluginHost redactor
        // processes this complete bounded message before reporting it.
        message: format!(
            "plugin supervisor handshake failed: {cause}; before_kill={before_kill:?}; reaped={status}; {stderr}"
        ),
    }
}

fn observe_exit(pid: u32) -> io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt as _;
    let pid = Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?)
        .ok_or_else(|| io::Error::other("invalid helper pid"))?;
    let Some(status) = rustix::process::waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    )?
    else {
        return Ok(None);
    };
    let raw = if let Some(code) = status.exit_status() {
        code << 8
    } else if let Some(signal) = status.terminating_signal() {
        signal | if status.dumped() { 0x80 } else { 0 }
    } else {
        return Err(io::Error::other("unexpected helper wait status"));
    };
    Ok(Some(ExitStatus::from_raw(raw)))
}

fn read_stderr(stderr: &impl std::os::fd::AsFd) -> String {
    // Tokio's child pipe is already nonblocking. Verify that contract before
    // reading; no background drain or inherited writer may extend retirement.
    let nonblocking = rustix::fs::fcntl_getfl(stderr)
        .is_ok_and(|flags| flags.contains(rustix::fs::OFlags::NONBLOCK));
    if !nonblocking {
        return "stderr capture unavailable: pipe is not nonblocking".to_owned();
    }
    let mut bytes = [0_u8; STDERR_BYTES];
    let mut used = 0;
    for _ in 0..32 {
        match rustix::io::read(stderr, &mut bytes[used..]) {
            Ok(0) if used < STDERR_BYTES => {
                return format!("stderr={}", String::from_utf8_lossy(&bytes[..used]));
            }
            Ok(count) => {
                used += count;
                if used == STDERR_BYTES {
                    break;
                }
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => break,
        }
    }
    // A truncated credential cannot be redacted as a complete known secret.
    // Report incomplete capture without publishing its potentially partial text.
    "stderr capture incomplete or exceeded 4096 bytes; content omitted".to_owned()
}

#[cfg(test)]
mod tests;
