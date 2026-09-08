mod catalog;
mod client_proof;
mod invocations;
mod lifecycle;
mod operations;
mod search;
mod transition;

use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::{
    CappedResponse, DeferredTool, McpCatalogEntry, McpClient, McpConnector, McpError, McpLimits,
    McpResponse, McpResponseLimits, McpResponseSlot, McpServerConfig, McpToolDefinition,
    OverflowSpool, ServerState, ServerStatus,
};
use catalog::{PreparedCatalog, prepare_catalog};
use rw_tools::CapabilityManifest;
use rw_types::McpServerId;

/// Aggregate server admission bound for each manager.
pub const MAX_SERVERS: usize = 64;

const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_CATALOG_ENTRY_BYTES: usize = 64 * 1024;
const MAX_SEARCH_RESULTS: usize = 32;

/// Boundary implemented by `rw-core`'s pinned TOON encoder.
pub trait StructuredResponseEncoder: Send + Sync {
    fn working_bytes(&self, value: &Value) -> Result<usize, McpError>;
    fn encode(&self, value: &Value) -> Result<Vec<u8>, McpError>;
    fn format(&self) -> &'static str;
}

/// Deterministic fallback useful for APIs and tests. Production injects TOON.
pub struct CompactJsonEncoder;

impl StructuredResponseEncoder for CompactJsonEncoder {
    fn working_bytes(&self, value: &Value) -> Result<usize, McpError> {
        let mut writer = rw_types::json_encoding::JsonWriter::count(
            rw_types::session_payload::MAX_SESSION_PAYLOAD_BYTES,
        );
        writer
            .serialize(value)
            .map_err(|error| McpError::Encoding(error.to_string()))?;
        writer
            .written()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or_else(|| McpError::Encoding("MCP JSON allocation overflow".into()))
    }
    fn encode(&self, value: &Value) -> Result<Vec<u8>, McpError> {
        let mut bytes = Vec::new();
        rw_types::json_encoding::JsonWriter::buffer(
            &mut bytes,
            rw_types::session_payload::MAX_SESSION_PAYLOAD_BYTES,
            1024,
        )
        .map_err(|error| McpError::Encoding(error.to_string()))?
        .serialize(value)
        .map_err(|error| McpError::Encoding(error.to_string()))?;
        Ok(bytes)
    }
    fn format(&self) -> &'static str {
        "json"
    }
}

struct ServerEntry {
    config: McpServerConfig,
    state: ServerState,
    client: Option<Arc<dyn McpClient>>,
    tools: McpResponse<Vec<Value>>,
    resources: McpResponse<Vec<Value>>,
    prompts: McpResponse<Vec<Value>>,
    catalog_fingerprint: Option<blake3::Hash>,
    pending_catalog: Option<PreparedCatalog>,
    generation: u64,
    transition: Option<Arc<transition::Transition>>,
}

impl ServerEntry {
    fn catalog_valid(&self) -> bool {
        self.client
            .as_ref()
            .is_none_or(|client| client.catalog_valid())
    }

    fn ready(&self) -> bool {
        self.config.enabled && matches!(self.state, ServerState::Ready) && self.catalog_valid()
    }

    fn status(&self) -> ServerStatus {
        ServerStatus {
            id: self.config.id.clone(),
            enabled: self.config.enabled,
            state: if self.config.enabled
                && !self.catalog_valid()
                && matches!(
                    self.state,
                    ServerState::Ready | ServerState::ApprovalRequired
                ) {
                ServerState::Failed {
                    message: "MCP catalog connection requires reconnection and schema review"
                        .to_owned(),
                }
            } else {
                self.state.clone()
            },
            tool_count: self.tools.len(),
            resource_count: self.resources.len(),
            prompt_count: self.prompts.len(),
        }
    }
}

/// Concurrent, deterministic registry with bounded MCP server admission.
#[derive(Clone)]
pub struct McpManager {
    inner: Arc<ManagerState>,
}

struct ManagerState {
    connector: Arc<dyn McpConnector>,
    spool: Arc<dyn OverflowSpool>,
    encoder: Arc<dyn StructuredResponseEncoder>,
    limits: McpLimits,
    operations: Arc<operations::Operations>,
    shutdown: std::sync::Mutex<Option<Arc<transition::Transition>>>,
    servers: RwLock<BTreeMap<McpServerId, ServerEntry>>,
    tool_capabilities: std::sync::RwLock<BTreeMap<McpServerId, crate::McpToolCapabilityOverrides>>,
}

impl McpManager {
    #[must_use]
    pub fn new(
        connector: Arc<dyn McpConnector>,
        spool: Arc<dyn OverflowSpool>,
        encoder: Arc<dyn StructuredResponseEncoder>,
        limits: McpLimits,
    ) -> Self {
        Self {
            inner: Arc::new(ManagerState {
                operations: Arc::new(operations::Operations::default()),
                shutdown: std::sync::Mutex::new(None),
                connector,
                spool,
                encoder,
                limits,
                servers: RwLock::new(BTreeMap::new()),
                tool_capabilities: std::sync::RwLock::new(BTreeMap::new()),
            }),
        }
    }

    pub async fn register(&self, config: McpServerConfig) -> Result<(), McpError> {
        self.register_with_state(config, false).await
    }

    /// Registers configured enablement without opening a connection. This is
    /// used by interactive hosts whose ordinary local startup must remain
    /// credential- and network-idle; a later explicit `set_enabled(true)` is
    /// the connection boundary.
    pub async fn register_deferred(&self, config: McpServerConfig) -> Result<(), McpError> {
        self.register_with_state(config, true).await
    }

    async fn register_with_state(
        &self,
        config: McpServerConfig,
        defer_connection: bool,
    ) -> Result<(), McpError> {
        let mut servers = self.inner.servers.write().await;
        self.inner.operations.ensure_open()?;
        if servers.len() >= MAX_SERVERS {
            return Err(McpError::Policy("MCP server capacity exhausted".to_owned()));
        }
        if servers.contains_key(&config.id) {
            return Err(McpError::DuplicateServer(config.id));
        }
        self.inner
            .tool_capabilities
            .write()
            .map_err(|_| McpError::Policy("MCP capability policy lock was poisoned".to_owned()))?
            .insert(config.id.clone(), config.tool_capabilities.clone());
        let state = if config.enabled && !defer_connection {
            ServerState::Connecting
        } else {
            ServerState::Disabled
        };
        servers.insert(
            config.id.clone(),
            ServerEntry {
                config,
                state,
                client: None,
                tools: McpResponse::empty(),
                resources: McpResponse::empty(),
                prompts: McpResponse::empty(),
                catalog_fingerprint: None,
                pending_catalog: None,
                generation: 0,
                transition: None,
            },
        );
        Ok(())
    }

    /// Removes a server that has not been enabled. This is deliberately
    /// narrower than a general unregister operation: callers use it to roll
    /// back a live registration when durable configuration persistence fails.
    pub async fn unregister_disabled(&self, server: &McpServerId) -> Result<(), McpError> {
        let mut servers = self.inner.servers.write().await;
        let entry = servers
            .get(server)
            .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
        if entry.config.enabled
            || entry.client.is_some()
            || !matches!(entry.state, ServerState::Disabled)
        {
            return Err(McpError::Policy(
                "only a disabled MCP server can be unregistered".to_owned(),
            ));
        }
        servers.remove(server);
        self.inner
            .tool_capabilities
            .write()
            .map_err(|_| McpError::Policy("MCP capability policy lock was poisoned".to_owned()))?
            .remove(server);
        Ok(())
    }

    /// Resolves permission effects without awaiting so core can classify an
    /// invocation before the permission gate runs. Unknown or poisoned state
    /// remains network + execute.
    #[must_use]
    pub fn tool_capabilities(&self, server: &McpServerId, tool: &str) -> CapabilityManifest {
        self.inner
            .tool_capabilities
            .read()
            .ok()
            .and_then(|policies| policies.get(server).map(|policy| policy.resolve(tool)))
            .unwrap_or_else(McpToolDefinition::restrictive_capabilities)
    }

    /// Re-lists tools. Changed schemas stay pending until `approve_changes` is true.
    pub async fn refresh_tools(
        &self,
        server: &McpServerId,
        approve_changes: bool,
    ) -> Result<bool, McpError> {
        let client = self.client(server).await?;
        let refresh_client = Arc::clone(&client);
        let tools = self
            .invoke(server, Arc::clone(&client), async move {
                prepare_catalog(
                    refresh_client
                        .list_tools(McpResponseSlot::new(refresh_client.response_limits())?)
                        .await?,
                )
                .await
            })
            .await?;
        let fingerprint = tools.fingerprint;
        let mut servers = self.inner.servers.write().await;
        let entry = servers
            .get_mut(server)
            .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
        if !entry.catalog_valid()
            || !entry
                .client
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &client))
        {
            return Err(McpError::NotConnected(server.clone()));
        }
        if entry.catalog_fingerprint == Some(fingerprint) {
            return Ok(false);
        }
        if approve_changes {
            entry.tools = tools.values;
            entry.catalog_fingerprint = Some(fingerprint);
            entry.pending_catalog = None;
        } else {
            entry.pending_catalog = Some(tools);
            entry.state = ServerState::ApprovalRequired;
        }
        Ok(true)
    }

    pub async fn approve_pending_tools(&self, server: &McpServerId) -> Result<bool, McpError> {
        let mut servers = self.inner.servers.write().await;
        self.inner.operations.ensure_open()?;
        let entry = servers
            .get_mut(server)
            .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
        if !entry.config.enabled {
            return Err(McpError::Disabled(server.clone()));
        }
        if entry.client.is_none() || !entry.catalog_valid() {
            return Err(McpError::NotConnected(server.clone()));
        }
        let Some(tools) = entry.pending_catalog.take() else {
            return Ok(false);
        };
        entry.catalog_fingerprint = Some(tools.fingerprint);
        entry.tools = tools.values;
        entry.state = ServerState::Ready;
        Ok(true)
    }

    #[must_use]
    pub async fn statuses(&self) -> Vec<ServerStatus> {
        self.inner
            .servers
            .read()
            .await
            .values()
            .map(ServerEntry::status)
            .collect()
    }

    /// Name + one-line description only: no input schemas or annotations.
    #[must_use]
    pub async fn deferred_tool_index(&self) -> Vec<DeferredTool> {
        let servers = self.inner.servers.read().await;
        let mut index = Vec::new();
        for (server, entry) in &*servers {
            if !entry.config.enabled
                || !entry.config.defer_tools
                || !matches!(entry.state, ServerState::Ready)
                || !entry.catalog_valid()
            {
                continue;
            }
            for tool in &entry.tools {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    index.push(DeferredTool {
                        server: server.clone(),
                        name: name.to_owned(),
                        description: one_line(
                            tool.get("description")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        ),
                    });
                }
            }
        }
        index
    }

    /// Exact provider-context fragment used for measured deferred-loading tests.
    pub async fn deferred_prompt(&self) -> Result<String, McpError> {
        serde_json::to_string(&self.deferred_tool_index().await)
            .map_err(|error| McpError::Encoding(error.to_string()))
    }

    pub async fn resources(&self) -> Vec<McpCatalogEntry> {
        self.catalog_entries("resources").await
    }

    pub async fn prompts(&self) -> Vec<McpCatalogEntry> {
        self.catalog_entries("prompts").await
    }

    async fn catalog_entries(&self, kind: &str) -> Vec<McpCatalogEntry> {
        let servers = self.inner.servers.read().await;
        let mut result = Vec::new();
        for (server, entry) in &*servers {
            if !entry.ready() {
                continue;
            }
            let values = if kind == "resources" {
                &entry.resources
            } else {
                &entry.prompts
            };
            for value in values {
                result.push(McpCatalogEntry {
                    server: server.clone(),
                    name: value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    description: value
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    uri: value.get("uri").and_then(Value::as_str).map(str::to_owned),
                });
            }
        }
        result
    }

    pub async fn call_tool(
        &self,
        server: &McpServerId,
        name: &str,
        arguments: Value,
    ) -> Result<CappedResponse, McpError> {
        let approved = self
            .inner
            .servers
            .read()
            .await
            .get(server)
            .is_some_and(|entry| {
                entry
                    .tools
                    .iter()
                    .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            });
        if !approved {
            return Err(McpError::Protocol(
                "tool is not in the approved MCP catalog".to_owned(),
            ));
        }
        let client = self.client(server).await?;
        let manager = self.clone();
        let id = server.clone();
        let name = name.to_owned();
        self.invoke(server, Arc::clone(&client), async move {
            let value = client
                .call_tool(
                    &name,
                    arguments,
                    McpResponseSlot::new(client.response_limits())?,
                )
                .await?;
            manager
                .cap(
                    &id,
                    "tool result",
                    value,
                    crate::McpResponseUse::CanonicalTool,
                )
                .await
        })
        .await
    }

    pub async fn read_resource(
        &self,
        server: &McpServerId,
        uri: &str,
    ) -> Result<CappedResponse, McpError> {
        let client = self.client(server).await?;
        let manager = self.clone();
        let id = server.clone();
        let uri = uri.to_owned();
        self.invoke(server, Arc::clone(&client), async move {
            let value = client
                .read_resource(&uri, McpResponseSlot::new(client.response_limits())?)
                .await?;
            manager
                .cap(&id, "resource", value, crate::McpResponseUse::CanonicalTool)
                .await
        })
        .await
    }

    pub async fn get_prompt(
        &self,
        server: &McpServerId,
        name: &str,
        arguments: Value,
        destination: crate::McpResponseUse,
    ) -> Result<CappedResponse, McpError> {
        let client = self.client(server).await?;
        let manager = self.clone();
        let id = server.clone();
        let name = name.to_owned();
        self.invoke(server, Arc::clone(&client), async move {
            let value = client
                .get_prompt(
                    &name,
                    arguments,
                    McpResponseSlot::new(client.response_limits())?,
                )
                .await?;
            manager.cap(&id, "prompt", value, destination).await
        })
        .await
    }

    async fn client(&self, server: &McpServerId) -> Result<Arc<dyn McpClient>, McpError> {
        let servers = self.inner.servers.read().await;
        let entry = servers
            .get(server)
            .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
        if !entry.config.enabled {
            return Err(McpError::Disabled(server.clone()));
        }
        if !entry.ready() {
            return Err(McpError::NotConnected(server.clone()));
        }
        entry
            .client
            .clone()
            .ok_or_else(|| McpError::NotConnected(server.clone()))
    }

    async fn cap(
        &self,
        server: &McpServerId,
        operation: &str,
        value: McpResponse<Value>,
        destination: crate::McpResponseUse,
    ) -> Result<CappedResponse, McpError> {
        let mut encoded = crate::encoding::encode(Arc::clone(&self.inner.encoder), value).await?;
        if let crate::McpResponseUse::Inline { max_bytes } = destination {
            if max_bytes > self.inner.limits.response_bytes || encoded.bytes.len() > max_bytes {
                return Err(McpError::Encoding(
                    "inline MCP result exceeds its destination limit".into(),
                ));
            }
            // Retained through bounded JSON encoding and escaped command framing.
            encoded.retained.resize(
                encoded
                    .bytes
                    .capacity()
                    .saturating_add(max_bytes.saturating_mul(3))
                    .saturating_add(4096),
            )?;
            return self.compact_response(encoded, None);
        }
        let overflow = if encoded.bytes.len() > self.inner.limits.response_bytes {
            Some(self.inner.spool.write(server, operation, encoded).await?)
        } else {
            return self.compact_response(encoded, None);
        };
        let summary = McpResponseSlot::new(McpResponseLimits::new(64 * 1024)?)?
            .adopt(json!({"truncated":true,"overflow":overflow}))
            .await?;
        let encoded = crate::encoding::encode(Arc::clone(&self.inner.encoder), summary).await?;
        self.compact_response(encoded, overflow)
    }

    fn compact_response(
        &self,
        mut encoded: crate::EncodedPayload,
        overflow: Option<rw_types::SessionPayloadReference>,
    ) -> Result<CappedResponse, McpError> {
        if encoded.bytes.len() > self.inner.limits.response_bytes {
            return Err(McpError::Encoding(
                "overflow reference exceeds MCP response cap".into(),
            ));
        }
        // Covers the protected framing copy and compact metadata until core admits its result.
        encoded
            .retained
            .ensure(
                encoded
                    .bytes
                    .capacity()
                    .saturating_mul(3)
                    .saturating_add(4096),
            )
            .map_err(|error| McpError::Encoding(error.to_string()))?;
        let text = String::from_utf8(encoded.bytes)
            .map_err(|_| McpError::Encoding("MCP response is not UTF-8".into()))?;
        let payloads = rw_tools::ToolResultPayloads::retained(
            overflow.iter().cloned().collect(),
            Arc::new((encoded.retained, Arc::clone(&self.inner.spool))),
        )
        .map_err(|error| McpError::Spool(error.to_string()))?;
        Ok(CappedResponse {
            encoded: text,
            format: self.inner.encoder.format().to_owned(),
            truncated: overflow.is_some(),
            overflow,
            payloads,
        })
    }
}

fn status_message(error: &McpError) -> String {
    match error {
        McpError::EffectsUnsettled { .. } => "MCP effects are unsettled".to_owned(),
        McpError::PendingLogin { .. } => "MCP login is required".to_owned(),
        McpError::Disabled(_) => "MCP server is disabled".to_owned(),
        McpError::NotConnected(_) => "MCP server is not connected".to_owned(),
        McpError::Policy(_) => "MCP transport policy rejected the connection".to_owned(),
        McpError::Transport | McpError::Protocol(_) => "MCP protocol operation failed".to_owned(),
        McpError::Encoding(_) => "MCP response encoding failed".to_owned(),
        McpError::Spool(_) => "MCP overflow storage failed".to_owned(),
        McpError::InvalidCommand(_) | McpError::DuplicateServer(_) | McpError::UnknownServer(_) => {
            "MCP configuration is invalid".to_owned()
        }
    }
}

async fn load_catalog(
    client: &dyn McpClient,
) -> Result<
    (
        McpResponse<Vec<Value>>,
        McpResponse<Vec<Value>>,
        McpResponse<Vec<Value>>,
    ),
    McpError,
> {
    let (tools, resources, prompts) = tokio::join!(
        client.list_tools(McpResponseSlot::new(client.response_limits())?),
        client.list_resources(McpResponseSlot::new(client.response_limits())?),
        client.list_prompts(McpResponseSlot::new(client.response_limits())?)
    );
    Ok((tools?, resources?, prompts?))
}

fn one_line(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(160).collect()
}

fn definition(
    server: &McpServerId,
    tool: &Value,
    overrides: &crate::McpToolCapabilityOverrides,
) -> Option<McpToolDefinition> {
    let name = tool.get("name")?.as_str()?.to_owned();
    Some(McpToolDefinition {
        server: server.clone(),
        capabilities: overrides.resolve(&name),
        name,
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        input_schema: tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object"})),
    })
}

fn sanitize_catalog(
    mut values: McpResponse<Vec<Value>>,
) -> Result<McpResponse<Vec<Value>>, McpError> {
    if values.len() > MAX_CATALOG_ENTRIES {
        return Err(McpError::Protocol(
            "MCP catalog entry limit exceeded".into(),
        ));
    }
    for value in &mut values.value {
        let mut bytes = rw_types::json_encoding::JsonWriter::count(MAX_CATALOG_ENTRY_BYTES);
        bytes
            .serialize(value)
            .map_err(|_| McpError::Protocol("MCP catalog entry size limit exceeded".into()))?;
        for key in ["name", "description", "uri"] {
            let replacement = value.get(key).and_then(Value::as_str).map(|text| {
                let cap = if key == "description" { 512 } else { 256 };
                text.chars().take(cap).collect::<String>()
            });
            if let Some(text) = replacement {
                value[key] = Value::String(text);
            }
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests;
