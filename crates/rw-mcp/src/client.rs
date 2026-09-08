mod calls;
mod catalog;
mod closure;
mod inbound;
mod ingress;
mod start;
mod transport;
pub use inbound::McpInboundRouter;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use rmcp::{
    model::{CallToolRequestParams, GetPromptRequestParams, JsonObject, ReadResourceRequestParams},
    service::{RoleClient, RunningService},
};
use rw_tools::{
    ProtocolChildLauncher, ProtocolChildRequest, ProtocolProcessHandle, ProtocolSandboxPolicy,
};
use rw_types::McpServerId;
use serde_json::Value;

use crate::McpTransportConfig;
use crate::{McpError, McpResponse, McpResponseLimits, McpResponseSlot, McpServerConfig};

const MAX_PAGINATED_ENTRIES: usize = 256;

/// Host gate invoked before any MCP connection is opened. Implementations bind
/// approval to the complete non-secret launch/endpoint configuration and its
/// trusted origin, and therefore re-prompt when that configuration changes.
#[async_trait]
pub trait McpConnectionApprovalPolicy: Send + Sync {
    async fn approve(&self, config: &McpServerConfig) -> Result<(), McpError>;
}

#[async_trait]
pub trait McpClient: Send + Sync {
    /// Whether the connected catalog snapshot remains authoritative.
    /// A notification that revokes it requires explicit reconnection and review.
    fn catalog_valid(&self) -> bool;
    fn response_limits(&self) -> McpResponseLimits;
    async fn list_tools(&self, slot: McpResponseSlot) -> Result<McpResponse<Vec<Value>>, McpError>;
    async fn list_resources(
        &self,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<Value>>, McpError>;
    async fn list_prompts(
        &self,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<Value>>, McpError>;
    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, McpError>;
    async fn read_resource(
        &self,
        uri: &str,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, McpError>;
    async fn get_prompt(
        &self,
        name: &str,
        arguments: Value,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, McpError>;
    async fn close(&self, timeout: Duration) -> Result<(), McpError>;
}

#[async_trait]
pub trait McpConnector: Send + Sync {
    /// Production implementations enforce destination/redirect/proxy policy, credential
    /// secrecy, and a bounded frame/body before rmcp decodes untrusted bytes.
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError>;
}

/// Host-owned OAuth boundary. Implementations resolve only vault references and
/// must bind returned tokens to the requested resource/audience.
#[async_trait]
pub trait McpAuthorizationProvider: Send + Sync {
    async fn token(
        &self,
        server: &McpServerId,
        resource: &str,
    ) -> Result<Option<crate::SecretToken>, McpError>;
}

/// Production stdio connector generic over the host's sandboxed launcher.
pub struct SandboxedStdioConnector<L> {
    launcher: L,
    approval: Arc<dyn McpConnectionApprovalPolicy>,
}

impl<L> SandboxedStdioConnector<L> {
    #[must_use]
    pub fn new(launcher: L, approval: Arc<dyn McpConnectionApprovalPolicy>) -> Self {
        Self { launcher, approval }
    }
}

#[async_trait]
impl<L> McpConnector for SandboxedStdioConnector<L>
where
    L: ProtocolChildLauncher + Send + Sync,
{
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
        let McpTransportConfig::Stdio {
            executable,
            args,
            working_directory,
            environment,
            sandbox,
        } = &config.transport
        else {
            return Err(McpError::Policy(
                "streamable HTTP requires the host-injected guarded connector".to_owned(),
            ));
        };
        self.approval.approve(config).await?;
        let ingress = ingress::Ingress::new(McpInboundRouter::default())?;
        let spawned = self
            .launcher
            .spawn(&ProtocolChildRequest {
                executable: executable.clone(),
                args: args.clone(),
                working_directory: working_directory.clone(),
                environment: environment.clone(),
                sandbox: ProtocolSandboxPolicy {
                    read_roots: sandbox.read_roots.clone(),
                    write_roots: sandbox.write_roots.clone(),
                    allowed_domains: sandbox.allowed_domains.clone(),
                },
            })
            .await
            .map_err(|error| McpError::Policy(error.to_string()))?;
        let rw_tools::SpawnedProtocolChild {
            stdin,
            stdout,
            handle,
        } = spawned;
        let transport = match ingress::stdio::StdioTransport::new(
            Box::pin(stdout),
            Box::pin(stdin),
            Arc::clone(&ingress),
        ) {
            Ok(transport) => transport,
            Err(error) => {
                closure::retire_process(handle, Duration::from_secs(3))
                    .await
                    .map_err(|_| McpError::EffectsUnsettled {
                        server: config.id.clone(),
                        message: "MCP framing admission failed without native process settlement"
                            .into(),
                    })?;
                return Err(error);
            }
        };
        start::start(
            config.id.clone(),
            transport::ClientTransport::Stdio(transport),
            ingress,
            Some(handle),
        )
        .await
    }
}

#[cfg(feature = "test-support")]
mod test_connector;
#[cfg(feature = "test-support")]
pub use test_connector::TestOnlyUnsandboxedStdioConnector;

struct RmcpClient {
    server: McpServerId,
    peer: rmcp::Peer<RoleClient>,
    inbound: McpInboundRouter,
    closure: closure::ConnectionClosure,
    ingress: Arc<ingress::Ingress>,
}

impl RmcpClient {
    async fn new(
        server: McpServerId,
        service: RunningService<RoleClient, McpInboundRouter>,
        child: Option<Box<dyn ProtocolProcessHandle>>,
        ingress: Arc<ingress::Ingress>,
    ) -> Self {
        let client = Self {
            server,
            peer: service.peer().clone(),
            inbound: service.service().clone(),
            closure: closure::ConnectionClosure::new(service, child, Arc::clone(&ingress)),
            ingress,
        };
        // The manager owns reviewed catalogs; rmcp must not retain a second,
        // uncharged response cache or replay stale remote bodies after errors.
        client
            .peer
            .set_response_cache_config(rmcp::ClientCacheConfig::disabled())
            .await;
        client
    }

    fn peer(&self) -> Result<rmcp::Peer<RoleClient>, McpError> {
        if self.closure.is_closed() || !self.catalog_valid() {
            Err(McpError::NotConnected(self.server.clone()))
        } else {
            Ok(self.peer.clone())
        }
    }
}

/// Opens a policy-approved raw HTTP connection under admitted transport ownership.
pub async fn connect_http(
    server: McpServerId,
    endpoint: String,
    token: Option<crate::SecretToken>,
    client: Arc<dyn crate::McpHttpClient>,
    capacity: usize,
) -> Result<Arc<dyn McpClient>, McpError> {
    let ingress = ingress::Ingress::new(McpInboundRouter::default())?;
    let transport =
        ingress::http::HttpTransport::new(endpoint, token, client, Arc::clone(&ingress), capacity)?;
    start::start(
        server,
        transport::ClientTransport::Http(transport),
        ingress,
        None,
    )
    .await
}

fn json_object(value: Value) -> Result<JsonObject, McpError> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(McpError::Protocol(
            "MCP arguments must be a JSON object".to_owned(),
        )),
    }
}

fn protocol(_error: impl std::fmt::Display) -> McpError {
    protocol_failure()
}

fn protocol_failure() -> McpError {
    McpError::Protocol("remote MCP protocol operation failed".to_owned())
}

#[async_trait]
impl McpClient for RmcpClient {
    fn catalog_valid(&self) -> bool {
        self.inbound.catalog_valid() && !self.peer.is_transport_closed()
    }

    fn response_limits(&self) -> McpResponseLimits {
        McpResponseLimits::WIRE
    }

    async fn list_tools(&self, slot: McpResponseSlot) -> Result<McpResponse<Vec<Value>>, McpError> {
        self.catalog(catalog::Catalog::Tools, slot).await
    }
    async fn list_resources(
        &self,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<Value>>, McpError> {
        self.catalog(catalog::Catalog::Resources, slot).await
    }
    async fn list_prompts(
        &self,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<Value>>, McpError> {
        self.catalog(catalog::Catalog::Prompts, slot).await
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, McpError> {
        let params =
            CallToolRequestParams::new(name.to_owned()).with_arguments(json_object(arguments)?);
        let request = rmcp::model::CallToolRequest::new(params);
        self.value_request(request.into(), calls::ResultKind::Tool, slot)
            .await
    }
    async fn read_resource(
        &self,
        uri: &str,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, McpError> {
        let request = rmcp::model::ReadResourceRequest::new(ReadResourceRequestParams::new(uri));
        self.value_request(request.into(), calls::ResultKind::Resource, slot)
            .await
    }
    async fn get_prompt(
        &self,
        name: &str,
        arguments: Value,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, McpError> {
        let request = rmcp::model::GetPromptRequest::new(
            GetPromptRequestParams::new(name).with_arguments(json_object(arguments)?),
        );
        self.value_request(request.into(), calls::ResultKind::Prompt, slot)
            .await
    }

    async fn close(&self, timeout: Duration) -> Result<(), McpError> {
        self.closure
            .close(timeout)
            .await
            .map_err(|message| McpError::EffectsUnsettled {
                server: self.server.clone(),
                message: message.to_string(),
            })
    }
}

/// Useful for hosts that need to prepare non-secret HTTP header metadata.
#[must_use]
pub fn sorted_headers(
    headers: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    headers.into_iter().collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use std::io;

    #[cfg(unix)]
    struct AllowConnection;

    #[cfg(unix)]
    #[async_trait]
    impl McpConnectionApprovalPolicy for AllowConnection {
        async fn approve(&self, _: &McpServerConfig) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[cfg(unix)]
    struct ShellLauncher(&'static str);

    #[cfg(unix)]
    struct ShellHandle(tokio::process::Child);

    #[cfg(unix)]
    #[async_trait]
    impl ProtocolProcessHandle for ShellHandle {
        async fn observe_exit(
            &mut self,
            deadline: Duration,
        ) -> io::Result<Option<std::process::ExitStatus>> {
            match tokio::time::timeout(deadline, self.0.wait()).await {
                Ok(status) => status.map(Some),
                Err(_) => Ok(None),
            }
        }

        async fn terminate_and_reap(&mut self, deadline: Duration) -> io::Result<()> {
            let _ = self.0.start_kill();
            tokio::time::timeout(deadline, self.0.wait())
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "test child did not exit")
                })??;
            Ok(())
        }
    }

    #[cfg(unix)]
    #[async_trait]
    impl ProtocolChildLauncher for ShellLauncher {
        async fn spawn(
            &self,
            _: &ProtocolChildRequest,
        ) -> io::Result<rw_tools::SpawnedProtocolChild> {
            let mut child = tokio::process::Command::new("/bin/sh")
                .args(["-c", self.0])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("test stdin unavailable"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("test stdout unavailable"))?;
            Ok(rw_tools::SpawnedProtocolChild {
                stdin,
                stdout,
                handle: Box::new(ShellHandle(child)),
            })
        }
    }

    #[cfg(unix)]
    fn stdio_config() -> McpServerConfig {
        McpServerConfig {
            id: McpServerId::new("fixture").expect("server id"),
            transport: McpTransportConfig::Stdio {
                executable: "/bin/sh".into(),
                args: vec![],
                working_directory: None,
                environment: vec![],
                sandbox: crate::McpStdioSandboxPolicy::default(),
            },
            enabled: true,
            defer_tools: true,
            tool_capabilities: crate::McpToolCapabilityOverrides::default(),
        }
    }

    #[test]
    fn request_arguments_transfer_existing_backing_and_reject_non_objects() {
        let text = "owned argument".repeat(8192);
        let pointer = text.as_ptr();
        let mut fields = JsonObject::new();
        fields.insert("text".into(), Value::String(text));
        let transferred = json_object(Value::Object(fields)).expect("object arguments");
        assert_eq!(
            transferred["text"].as_str().expect("text").as_ptr(),
            pointer
        );
        assert!(json_object(Value::Array(vec![])).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_connector_reports_bounded_natural_exit_without_child_stderr() {
        let connector = SandboxedStdioConnector::new(
            ShellLauncher("echo TOPSECRET >&2; exit 23"),
            Arc::new(AllowConnection),
        );
        let error = connector
            .connect(&stdio_config())
            .await
            .err()
            .expect("early exit must fail");
        let message = error.to_string();
        assert!(message.contains("exit status: 23"));
        assert!(!message.contains("TOPSECRET"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_connector_keeps_live_transport_failure_generic() {
        let connector = SandboxedStdioConnector::new(
            ShellLauncher("exec 1>&-; exec sleep 10"),
            Arc::new(AllowConnection),
        );
        let error = connector
            .connect(&stdio_config())
            .await
            .err()
            .expect("closed live transport must fail");
        assert_eq!(
            error.to_string(),
            "MCP protocol error: remote MCP protocol operation failed"
        );
    }
}
