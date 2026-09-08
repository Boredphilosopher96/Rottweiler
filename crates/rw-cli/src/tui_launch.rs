//! Required process contract shared by local, remote, and historical clients.
use crate::supervisor::{ChildSpec, StdioMode};
use rw_core::SequenceId;
use std::{collections::BTreeMap, ffi::OsString, path::Path, process::Stdio};
use std::{io, os::unix::process::ExitStatusExt as _, process::ExitStatus};

pub(super) const TUI_RECYCLE_EXIT_CODE: i32 = 75;

pub(super) fn tui_exit_is_user_close(status: ExitStatus) -> bool {
    status.success()
        || status.code() == Some(130)
        || status.signal() == Some(rustix::process::Signal::INT.as_raw())
}

pub(super) fn tui_exit_is_recycle(status: ExitStatus) -> bool {
    status.code() == Some(TUI_RECYCLE_EXIT_CODE)
}

pub(super) struct TuiLaunch<'a> {
    pub executable: &'a Path,
    pub socket: &'a Path,
    pub token_file: &'a Path,
    pub session_id: &'a str,
    pub last_seen_file: &'a Path,
    pub fork_operation_directory: &'a Path,
    pub keybindings: Option<&'a str>,
    pub theme: &'a str,
    pub replay: bool,
}

impl TuiLaunch<'_> {
    pub(super) fn spec(&self, last_seen: Option<SequenceId>) -> ChildSpec {
        let mut env = BTreeMap::from([
            (
                "ROTTWEILER_ENGINE_SOCKET".into(),
                self.socket.as_os_str().to_owned(),
            ),
            (
                "ROTTWEILER_ENGINE_TOKEN_FILE".into(),
                self.token_file.as_os_str().to_owned(),
            ),
            ("ROTTWEILER_SESSION_ID".into(), self.session_id.into()),
            (
                "ROTTWEILER_LAST_SEEN_FILE".into(),
                self.last_seen_file.as_os_str().to_owned(),
            ),
            (
                "ROTTWEILER_TUI_RECYCLE_STATE_FILE".into(),
                self.last_seen_file
                    .with_file_name("tui-recycle-state.json")
                    .into_os_string(),
            ),
            (
                "ROTTWEILER_FORK_OPERATION_DIRECTORY".into(),
                self.fork_operation_directory.as_os_str().to_owned(),
            ),
            (
                crate::parent_death::SUPERVISOR_PID_ENV.into(),
                std::process::id().to_string().into(),
            ),
            (
                "ROTTWEILER_REPLAY_MODE".into(),
                if self.replay { "1" } else { "0" }.into(),
            ),
            ("ROTTWEILER_TUI_THEME".into(), self.theme.into()),
        ]);
        if let Some(value) = self.keybindings {
            env.insert("ROTTWEILER_TUI_KEYBINDINGS".into(), value.into());
        }
        if let Some(sequence) = last_seen {
            env.insert(
                "ROTTWEILER_LAST_SEEN_SEQUENCE".into(),
                sequence.0.to_string().into(),
            );
        }
        ChildSpec {
            program: self.executable.to_owned(),
            args: vec![OsString::from(rw_types::release_contract::JS_HOST_TUI_ROLE)],
            env,
            stdio: StdioMode::Inherit,
            // The client remains in the foreground controlling-terminal group.
            new_process_group: false,
        }
    }
}

fn command(spec: &ChildSpec) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_remove("ROTTWEILER_TUI_KEYBINDINGS")
        .env_remove("ROTTWEILER_LAST_SEEN_SEQUENCE")
        .envs(&spec.env)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
}

/// The process task retains its child through kill and reap, including caller loss.
pub(super) struct TuiProcess {
    stop: tokio::sync::watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl TuiProcess {
    pub(super) fn start(launch: &TuiLaunch<'_>) -> Self {
        let spec = launch.spec(None);
        let (stop, stopping) = tokio::sync::watch::channel(false);
        Self {
            stop,
            task: Some(tokio::spawn(run(spec, stopping))),
        }
    }

    pub(super) async fn wait(&mut self) -> io::Result<()> {
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        let result = task.await;
        self.task = None;
        result.map_err(io::Error::other)?
    }

    pub(super) async fn shutdown(&mut self) -> io::Result<()> {
        let _ = self.stop.send(true);
        self.wait().await
    }
}

impl Drop for TuiProcess {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        // Dropping the join handle detaches its owner; it does not abort cleanup.
    }
}

async fn run(spec: ChildSpec, mut stopping: tokio::sync::watch::Receiver<bool>) -> io::Result<()> {
    let mut failures = 0_u32;
    loop {
        if *stopping.borrow() {
            return Ok(());
        }
        let mut child = command(&spec).spawn()?;
        let status = tokio::select! {
            status = child.wait() => status?,
            _ = stopping.changed() => {
                // Even a failed signal must retain ownership until the child is reaped.
                let _ = child.start_kill();
                child.wait().await?;
                return Ok(());
            }
        };
        if tui_exit_is_user_close(status) {
            return Ok(());
        }
        if tui_exit_is_recycle(status) {
            continue;
        }
        if failures == 5 {
            return Err(io::Error::other("TUI restart budget exhausted"));
        }
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(50_u64 << failures)) => {}
            _ = stopping.changed() => return Ok(()),
        }
        failures += 1;
    }
}

#[cfg(test)]
mod tests;
