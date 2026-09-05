use super::*;

#[cfg(test)]
pub(crate) struct TestDirectLauncher;

#[cfg(test)]
struct TestChildProcess {
    child: StdMutex<tokio::process::Child>,
    pid: Option<u32>,
}

#[cfg(test)]
#[async_trait]
impl SupervisedPluginProcess for TestChildProcess {
    async fn settle_effects(&self) -> Result<(), PluginProcessError> {
        self.reap().await?;
        rw_tools::terminate_and_wait_process_group(self.pid)
            .await
            .map_err(|error| PluginProcessError {
                message: error.to_string(),
            })
    }
    fn mark_capability_violation(&self, _violation: &crate::plugin::CapabilityViolation) {}
    fn kill_tree(&self) -> Result<(), PluginProcessError> {
        #[cfg(unix)]
        if let Some(pid) = self
            .pid
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        self.child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .start_kill()
            .map_err(|error| PluginProcessError {
                message: error.to_string(),
            })
    }
    async fn wait(&self) -> Result<Option<i32>, PluginProcessError> {
        loop {
            if let Some(status) = self
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_wait()
                .map_err(|error| PluginProcessError {
                    message: error.to_string(),
                })?
            {
                return Ok(status.code());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(test)]
#[async_trait]
impl PluginLauncher for TestDirectLauncher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        _profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginProcessError> {
        use std::os::unix::process::CommandExt;
        config.validate_executable_identity()?;
        let mut command = tokio::process::Command::new(config.executable());
        command
            .args(config.argv())
            .current_dir(config.cwd())
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for name in config.environment_allowlist() {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().map_err(|error| PluginProcessError {
            message: error.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| PluginProcessError {
            message: "missing stdin".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| PluginProcessError {
            message: "missing stdout".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| PluginProcessError {
            message: "missing stderr".to_owned(),
        })?;
        let pid = child.id();
        Ok(LaunchedPluginProcess {
            stdin: Box::pin(stdin),
            stdout: Box::pin(BufReader::new(stdout)),
            stderr: Box::pin(BufReader::new(stderr)),
            process: Arc::new(TestChildProcess {
                child: StdMutex::new(child),
                pid,
            }),
            executable_identity: config.executable_identity().clone(),
        })
    }
}
