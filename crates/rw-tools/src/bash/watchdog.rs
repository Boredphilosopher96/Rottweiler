use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::Duration;

use crate::registry::ToolError;

use super::execution_lease::ExecutionLease;

#[cfg(unix)]
pub(super) struct ParentDeathWatchdog {
    child: Child,
    control: Option<ChildStdin>,
    stderr_task: Option<tokio::task::JoinHandle<std::io::Result<u64>>>,
}

#[cfg(unix)]
pub(super) async fn arm_parent_death_watchdog(
    owner: &mut Option<ParentDeathWatchdog>,
    command_group_id: Option<u32>,
    execution_lease: Option<&ExecutionLease>,
) -> Result<(), ToolError> {
    arm_with_script(owner, command_group_id, execution_lease, WATCHDOG_SCRIPT).await
}

#[cfg(unix)]
const WATCHDOG_SCRIPT: &str = r#"
if ! : >&1; then exit 126; fi
printf 'ready\n' >&2
if [ -n "$2" ]; then printf '%s\n' "$$" > "$2"; fi
if IFS= read -r _; then exit 0; fi
if [ -n "$3" ]; then while [ -e "$3" ]; do sleep 0.01; done; fi
kill -KILL "-$1" 2>/dev/null || :
while kill -0 "-$1" 2>/dev/null; do sleep 0.01; done
"#;

#[cfg(unix)]
async fn arm_with_script(
    owner: &mut Option<ParentDeathWatchdog>,
    command_group_id: Option<u32>,
    execution_lease: Option<&ExecutionLease>,
    script: &str,
) -> Result<(), ToolError> {
    let group_id = command_group_id
        .ok_or_else(|| ToolError::Command("command process id was unavailable".to_owned()))?;

    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .arg("rottweiler-parent-death-watchdog")
        .arg(group_id.to_string())
        .arg(watchdog_test_pid_file())
        .arg(watchdog_test_pause_file())
        .stdin(Stdio::piped())
        .stdout(match execution_lease {
            Some(execution_lease) => execution_lease.watchdog_stdio()?,
            None => Stdio::null(),
        })
        .stderr(Stdio::piped());
    command.process_group(0);
    let child = command.spawn().map_err(|error| {
        ToolError::Command(format!("could not start command watchdog: {error}"))
    })?;
    *owner = Some(ParentDeathWatchdog {
        child,
        control: None,
        stderr_task: None,
    });
    let watchdog = owner
        .as_mut()
        .ok_or_else(|| ToolError::Command("watchdog owner is missing".to_owned()))?;
    watchdog.control =
        Some(watchdog.child.stdin.take().ok_or_else(|| {
            ToolError::Command("watchdog control pipe was not created".to_owned())
        })?);
    let stderr = watchdog
        .child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Command("watchdog ready pipe was not created".to_owned()))?;
    let mut stderr = BufReader::new(stderr);
    let (send_ready, ready) = tokio::sync::oneshot::channel();
    // The watchdog owns its readiness pipe before awaiting it. Caller loss
    // must not close stderr while the watchdog is publishing its ready frame.
    watchdog.stderr_task = Some(tokio::spawn(async move {
        let mut readiness = String::new();
        let result = stderr.read_line(&mut readiness).await.and_then(|_| {
            if readiness == "ready\n" {
                Ok(())
            } else {
                Err(std::io::Error::other("invalid watchdog readiness"))
            }
        });
        let _ = send_ready.send(result);
        let mut sink = tokio::io::sink();
        tokio::io::copy(&mut stderr, &mut sink).await
    }));
    if !matches!(
        tokio::time::timeout(Duration::from_secs(2), ready).await,
        Ok(Ok(Ok(())))
    ) {
        return Err(ToolError::Command(
            "command watchdog did not confirm its execution lease".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(all(unix, test))]
pub(super) fn watchdog_test_pid_file() -> String {
    std::env::var("ROTTWEILER_WATCHDOG_TEST_PID_FILE").unwrap_or_default()
}

#[cfg(all(unix, test))]
pub(super) fn watchdog_test_pause_file() -> String {
    std::env::var("ROTTWEILER_WATCHDOG_PAUSE_FILE").unwrap_or_default()
}

#[cfg(all(unix, not(test)))]
pub(super) fn watchdog_test_pid_file() -> String {
    String::new()
}

#[cfg(all(unix, not(test)))]
pub(super) fn watchdog_test_pause_file() -> String {
    String::new()
}

#[cfg(unix)]
impl ParentDeathWatchdog {
    pub(super) async fn wait_unexpected(&mut self) -> String {
        match self.child.wait().await {
            Ok(status) => status.to_string(),
            Err(error) => error.to_string(),
        }
    }

    pub(super) async fn disarm(&mut self) -> Result<(), ToolError> {
        let control_result = if let Some(mut control) = self.control.take() {
            let result = control
                .write_all(b"done\n")
                .await
                .map_err(|error| ToolError::Command(format!("could not disarm watchdog: {error}")));
            let _ = control.shutdown().await;
            result
        } else {
            Ok(())
        };
        let result = match tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(ToolError::Command(format!(
                "command watchdog failed while disarming: {status}"
            ))),
            Ok(Err(error)) => {
                tracing::error!(%error, "watchdog exit is unproven; retaining effect ownership");
                std::future::pending().await
            }
            Err(_) => {
                let _ = self.child.start_kill();
                if let Err(error) = self.child.wait().await {
                    tracing::error!(%error, "killed watchdog could not be reaped; retaining effect ownership");
                    std::future::pending::<()>().await;
                }
                Err(ToolError::Command(
                    "command watchdog required forced termination after disarm".to_owned(),
                ))
            }
        };
        if let Some(stderr_task) = &mut self.stderr_task {
            let _ = stderr_task.await;
        }
        control_result.and(result)
    }
}

#[cfg(not(unix))]
pub(super) struct ParentDeathWatchdog;

#[cfg(not(unix))]
pub(super) async fn arm_parent_death_watchdog(
    owner: &mut Option<ParentDeathWatchdog>,
    _command_group_id: Option<u32>,
    _execution_lease: Option<&ExecutionLease>,
) -> Result<(), ToolError> {
    *owner = Some(ParentDeathWatchdog);
    Ok(())
}

#[cfg(not(unix))]
impl ParentDeathWatchdog {
    pub(super) async fn wait_unexpected(&mut self) -> String {
        std::future::pending().await
    }

    pub(super) async fn disarm(&mut self) -> Result<(), ToolError> {
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests;
