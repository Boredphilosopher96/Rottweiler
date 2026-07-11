use std::{
    collections::BTreeMap,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
#[cfg(feature = "test-support")]
use rmcp::transport::TokioChildProcess;
#[cfg(feature = "test-support")]
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{
    ServiceExt as _,
    model::{
        CallToolRequestParams, GetPromptRequestParams, JsonObject, PaginatedRequestParams,
        ReadResourceRequestParams,
    },
    service::{RoleClient, RunningService},
};
use rw_tools::{
    ProtocolChildLauncher, ProtocolChildRequest, ProtocolProcessHandle, ProtocolSandboxPolicy,
};
use serde_json::Value;
#[cfg(feature = "test-support")]
use tokio::process::Command;
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::Mutex,
};

use crate::{McpError, McpServerConfig};
use crate::{McpTransportConfig, ServerId};

const MAX_PAGINATED_ENTRIES: usize = 256;
const MAX_STDIO_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Host gate invoked before any MCP connection is opened. Implementations bind
/// approval to the complete non-secret launch/endpoint configuration and its
/// trusted origin, and therefore re-prompt when that configuration changes.
#[async_trait]
pub trait McpConnectionApprovalPolicy: Send + Sync {
    async fn approve(&self, config: &McpServerConfig) -> Result<(), McpError>;
}

#[async_trait]
pub trait McpClient: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<Value>, McpError>;
    async fn list_resources(&self) -> Result<Vec<Value>, McpError>;
    async fn list_prompts(&self) -> Result<Vec<Value>, McpError>;
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError>;
    async fn read_resource(&self, uri: &str) -> Result<Value, McpError>;
    async fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, McpError>;
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
        server: &ServerId,
        resource: &str,
    ) -> Result<Option<crate::SecretToken>, McpError>;
}

/// Production Streamable HTTP connector. The HTTP implementation is injected;
/// `rw-mcp` never constructs reqwest or any ambient/default network client.
#[cfg(feature = "test-support")]
pub struct GuardedStreamableHttpConnector<C> {
    client: C,
    authorization: Arc<dyn McpAuthorizationProvider>,
    channel_capacity: usize,
    approval: Arc<dyn McpConnectionApprovalPolicy>,
}

#[cfg(feature = "test-support")]
impl<C> GuardedStreamableHttpConnector<C> {
    #[must_use]
    pub fn new(
        client: C,
        authorization: Arc<dyn McpAuthorizationProvider>,
        approval: Arc<dyn McpConnectionApprovalPolicy>,
    ) -> Self {
        Self {
            client,
            authorization,
            channel_capacity: 16,
            approval,
        }
    }

    #[must_use]
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity.clamp(1, 256);
        self
    }
}

#[async_trait]
#[cfg(feature = "test-support")]
impl<C> McpConnector for GuardedStreamableHttpConnector<C>
where
    C: StreamableHttpClient + Send + Sync,
{
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
        let McpTransportConfig::StreamableHttp { endpoint, oauth } = &config.transport else {
            return Err(McpError::Policy(
                "stdio MCP requires the sandboxed stdio connector".to_owned(),
            ));
        };
        self.approval.approve(config).await?;
        let token = if *oauth {
            self.authorization.token(&config.id, endpoint).await?
        } else {
            None
        };
        let mut transport_config = StreamableHttpClientTransportConfig::with_uri(endpoint.clone());
        transport_config.channel_buffer_capacity = self.channel_capacity;
        if let Some(token) = token {
            transport_config = transport_config.auth_header(token.expose().to_owned());
        }
        let transport =
            StreamableHttpClientTransport::with_client(self.client.clone(), transport_config);
        let service = ().serve(transport).await.map_err(protocol)?;
        Ok(Arc::new(RmcpClient::new(config.id.clone(), service, None)))
    }
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
            mut handle,
        } = spawned;
        if let Ok(service) =
            ().serve((BoundedLineReader::new(stdout, MAX_STDIO_FRAME_BYTES), stdin))
                .await
        {
            Ok(Arc::new(RmcpClient::new(
                config.id.clone(),
                service,
                Some(handle),
            )))
        } else {
            let _ = handle.terminate_and_reap(Duration::from_secs(3)).await;
            Err(protocol_failure())
        }
    }
}

/// Unsandboxed direct stdio connector for deterministic fixtures only.
/// Production must inject an `McpConnector` that owns sandbox/process-tree supervision.
#[cfg(feature = "test-support")]
pub struct TestOnlyUnsandboxedStdioConnector {
    policy: Arc<dyn McpConnectionApprovalPolicy>,
}

#[cfg(feature = "test-support")]
impl TestOnlyUnsandboxedStdioConnector {
    #[must_use]
    pub fn new(policy: Arc<dyn McpConnectionApprovalPolicy>) -> Self {
        Self { policy }
    }
}

struct RmcpClient {
    server: ServerId,
    service: Mutex<Option<RunningService<RoleClient, ()>>>,
    child: Mutex<Option<Box<dyn ProtocolProcessHandle>>>,
}

impl RmcpClient {
    fn new(
        server: ServerId,
        service: RunningService<RoleClient, ()>,
        child: Option<Box<dyn ProtocolProcessHandle>>,
    ) -> Self {
        Self {
            server,
            service: Mutex::new(Some(service)),
            child: Mutex::new(child),
        }
    }

    async fn peer(&self) -> Result<rmcp::Peer<RoleClient>, McpError> {
        self.service
            .lock()
            .await
            .as_ref()
            .map(|service| service.peer().clone())
            .ok_or_else(|| McpError::NotConnected(self.server.clone()))
    }
}

/// Composition-root bridge for the concrete guarded HTTP implementation.
/// Generic rmcp HTTP construction remains private/test-only.
#[doc(hidden)]
#[must_use]
pub fn boxed_running_http_client(
    server: ServerId,
    service: RunningService<RoleClient, ()>,
) -> Arc<dyn McpClient> {
    Arc::new(RmcpClient::new(server, service, None))
}

#[async_trait]
#[cfg(feature = "test-support")]
impl McpConnector for TestOnlyUnsandboxedStdioConnector {
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
        match &config.transport {
            McpTransportConfig::Stdio {
                executable,
                args,
                working_directory,
                environment,
                ..
            } => {
                self.policy.approve(config).await?;
                validate_stdio(executable, args, environment)?;
                let mut command = Command::new(executable);
                command
                    .env_clear()
                    .args(args)
                    .envs(environment.iter().cloned())
                    .kill_on_drop(true);
                if let Some(working_directory) = working_directory {
                    command.current_dir(working_directory);
                }
                let transport = TokioChildProcess::new(command)
                    .map_err(|error| McpError::Protocol(error.to_string()))?;
                let service = ().serve(transport).await.map_err(|_| protocol_failure())?;
                Ok(Arc::new(RmcpClient::new(config.id.clone(), service, None)))
            }
            McpTransportConfig::StreamableHttp { .. } => Err(McpError::Policy(
                "remote MCP requires a host-injected guarded McpConnector".to_owned(),
            )),
        }
    }
}

#[cfg(feature = "test-support")]
fn validate_stdio(
    executable: &std::path::Path,
    args: &[String],
    environment: &[(String, String)],
) -> Result<(), McpError> {
    if executable.as_os_str().is_empty() || executable.to_string_lossy().contains('\0') {
        return Err(McpError::InvalidCommand(
            "empty or NUL executable".to_owned(),
        ));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(McpError::InvalidCommand("argument contains NUL".to_owned()));
    }
    for (key, value) in environment {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(McpError::InvalidCommand(
                "invalid environment entry".to_owned(),
            ));
        }
    }
    Ok(())
}

fn json_object(value: &Value) -> Result<JsonObject, McpError> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| McpError::Protocol("MCP arguments must be a JSON object".to_owned()))
}

fn protocol(_error: impl std::fmt::Display) -> McpError {
    protocol_failure()
}

fn protocol_failure() -> McpError {
    McpError::Protocol("remote MCP protocol operation failed".to_owned())
}

struct BoundedLineReader<R> {
    inner: R,
    line_bytes: usize,
    max_line_bytes: usize,
}

impl<R> BoundedLineReader<R> {
    const fn new(inner: R, max_line_bytes: usize) -> Self {
        Self {
            inner,
            line_bytes: 0,
            max_line_bytes,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLineReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        destination: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let capacity = destination.remaining().min(8 * 1024);
        if capacity == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut buffer = [0_u8; 8 * 1024];
        let mut temporary = ReadBuf::new(&mut buffer[..capacity]);
        match Pin::new(&mut self.inner).poll_read(context, &mut temporary) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                let bytes = temporary.filled();
                let mut line_bytes = self.line_bytes;
                for byte in bytes {
                    if *byte == b'\n' {
                        line_bytes = 0;
                    } else {
                        line_bytes = line_bytes.saturating_add(1);
                        if line_bytes > self.max_line_bytes {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "MCP stdio frame exceeded its size cap",
                            )));
                        }
                    }
                }
                self.line_bytes = line_bytes;
                destination.put_slice(bytes);
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[async_trait]
impl McpClient for RmcpClient {
    async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let peer = self.peer().await?;
        let mut cursor = None;
        let mut values = Vec::new();
        loop {
            let page = peer
                .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .map_err(protocol)?;
            if values.len().saturating_add(page.tools.len()) > MAX_PAGINATED_ENTRIES {
                return Err(McpError::Protocol(
                    "MCP tool pagination limit exceeded".to_owned(),
                ));
            }
            values.extend(
                page.tools
                    .into_iter()
                    .map(|value| serde_json::to_value(value).map_err(protocol))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(values)
    }

    async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
        let peer = self.peer().await?;
        let mut cursor = None;
        let mut values = Vec::new();
        loop {
            let page = peer
                .list_resources(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .map_err(protocol)?;
            if values.len().saturating_add(page.resources.len()) > MAX_PAGINATED_ENTRIES {
                return Err(McpError::Protocol(
                    "MCP resource pagination limit exceeded".to_owned(),
                ));
            }
            values.extend(
                page.resources
                    .into_iter()
                    .map(|value| serde_json::to_value(value).map_err(protocol))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(values)
    }

    async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
        let peer = self.peer().await?;
        let mut cursor = None;
        let mut values = Vec::new();
        loop {
            let page = peer
                .list_prompts(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .map_err(protocol)?;
            if values.len().saturating_add(page.prompts.len()) > MAX_PAGINATED_ENTRIES {
                return Err(McpError::Protocol(
                    "MCP prompt pagination limit exceeded".to_owned(),
                ));
            }
            values.extend(
                page.prompts
                    .into_iter()
                    .map(|value| serde_json::to_value(value).map_err(protocol))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(values)
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let request =
            CallToolRequestParams::new(name.to_owned()).with_arguments(json_object(&arguments)?);
        let result = self
            .peer()
            .await?
            .call_tool(request)
            .await
            .map_err(protocol)?;
        serde_json::to_value(result).map_err(protocol)
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpError> {
        let result = self
            .peer()
            .await?
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(protocol)?;
        serde_json::to_value(result).map_err(protocol)
    }

    async fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let request = GetPromptRequestParams::new(name).with_arguments(json_object(&arguments)?);
        let result = self
            .peer()
            .await?
            .get_prompt(request)
            .await
            .map_err(protocol)?;
        serde_json::to_value(result).map_err(protocol)
    }

    async fn close(&self, timeout: Duration) -> Result<(), McpError> {
        let service_result = if let Some(mut service) = self.service.lock().await.take() {
            match service.close_with_timeout(timeout).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(McpError::ShutdownTimeout(self.server.clone())),
                Err(_) => Err(protocol_failure()),
            }
        } else {
            Ok(())
        };
        let child_result = if let Some(mut child) = self.child.lock().await.take() {
            child
                .terminate_and_reap(timeout)
                .await
                .map_err(|_| protocol_failure())
        } else {
            Ok(())
        };
        match (service_result, child_result) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        }
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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn bounded_stdio_reader_rejects_an_oversized_line_before_delivery() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let writing = tokio::spawn(async move {
            writer.write_all(b"12345\n").await.expect("write");
        });
        let mut reader = BoundedLineReader::new(reader, 4);
        let mut bytes = Vec::new();
        let error = reader
            .read_to_end(&mut bytes)
            .await
            .expect_err("oversized line");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writing.await.expect("writer");
    }
}
