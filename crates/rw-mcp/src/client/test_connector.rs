//! Explicit unsandboxed fixture launcher; framing still uses the real ingress.
use super::{McpInboundRouter, ingress, protocol, protocol_failure, start};
use crate::{
    McpClient, McpConnectionApprovalPolicy, McpConnector, McpError, McpServerConfig,
    McpTransportConfig,
};
use async_trait::async_trait;
use rw_tools::ProtocolProcessHandle;
use std::{io, sync::Arc, time::Duration};
use std::{path::Path, process::Stdio};

pub struct TestOnlyUnsandboxedStdioConnector {
    policy: Arc<dyn McpConnectionApprovalPolicy>,
}
impl TestOnlyUnsandboxedStdioConnector {
    #[must_use]
    pub fn new(policy: Arc<dyn McpConnectionApprovalPolicy>) -> Self {
        Self { policy }
    }
}
#[async_trait]
impl McpConnector for TestOnlyUnsandboxedStdioConnector {
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
        let McpTransportConfig::Stdio {
            executable,
            args,
            working_directory,
            environment,
            ..
        } = &config.transport
        else {
            return Err(McpError::Policy(
                "remote MCP requires a host-injected guarded McpConnector".into(),
            ));
        };
        self.policy.approve(config).await?;
        validate(executable, args, environment)?;
        let ingress = ingress::Ingress::new(McpInboundRouter::default())?;
        let mut command = tokio::process::Command::new(executable);
        command
            .env_clear()
            .args(args)
            .envs(environment.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(directory) = working_directory {
            command.current_dir(directory);
        }
        let mut child = command.spawn().map_err(protocol)?;
        let stdin = child.stdin.take().ok_or_else(protocol_failure)?;
        let stdout = child.stdout.take().ok_or_else(protocol_failure)?;
        let transport = ingress::stdio::StdioTransport::new(
            Box::pin(stdout),
            Box::pin(stdin),
            Arc::clone(&ingress),
        )?;
        start::start(
            config.id.clone(),
            super::transport::ClientTransport::Stdio(transport),
            ingress,
            Some(Box::new(Child(child))),
        )
        .await
    }
}
struct Child(tokio::process::Child);
#[async_trait]
impl ProtocolProcessHandle for Child {
    async fn observe_exit(
        &mut self,
        deadline: Duration,
    ) -> io::Result<Option<std::process::ExitStatus>> {
        match tokio::time::timeout(deadline, self.0.wait()).await {
            Ok(result) => result.map(Some),
            Err(_) => Ok(None),
        }
    }
    async fn terminate_and_reap(&mut self, deadline: Duration) -> io::Result<()> {
        let _ = self.0.start_kill();
        tokio::time::timeout(deadline, self.0.wait())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture child retirement timed out",
                )
            })??;
        Ok(())
    }
}
fn validate(
    executable: &Path,
    args: &[String],
    environment: &[(String, String)],
) -> Result<(), McpError> {
    if executable.as_os_str().is_empty() || executable.to_string_lossy().contains('\0') {
        return Err(McpError::InvalidCommand("empty or NUL executable".into()));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(McpError::InvalidCommand("argument contains NUL".into()));
    }
    if environment
        .iter()
        .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
    {
        return Err(McpError::InvalidCommand("invalid environment entry".into()));
    }
    Ok(())
}
