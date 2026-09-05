mod invocations;
mod lifecycle;
mod operations;
mod transition;

use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::{
    CappedResponse, DeferredTool, McpCatalogEntry, McpClient, McpConnector, McpError, McpLimits,
    McpServerConfig, McpToolDefinition, OverflowSpool, ServerState, ServerStatus,
};
use rw_tools::CapabilityManifest;
use rw_types::McpServerId;

const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_CATALOG_ENTRY_BYTES: usize = 64 * 1024;
const MAX_SEARCH_RESULTS: usize = 32;

/// Boundary implemented by `rw-core`'s pinned TOON encoder.
pub trait StructuredResponseEncoder: Send + Sync {
    fn encode(&self, value: &Value) -> Result<Vec<u8>, McpError>;
    fn format(&self) -> &'static str;
}

/// Deterministic fallback useful for APIs and tests. Production injects TOON.
pub struct CompactJsonEncoder;

impl StructuredResponseEncoder for CompactJsonEncoder {
    fn encode(&self, value: &Value) -> Result<Vec<u8>, McpError> {
        serde_json::to_vec(value).map_err(|error| McpError::Encoding(error.to_string()))
    }
    fn format(&self) -> &'static str {
        "json"
    }
}

struct ServerEntry {
    config: McpServerConfig,
    state: ServerState,
    client: Option<Arc<dyn McpClient>>,
    tools: Vec<Value>,
    resources: Vec<Value>,
    prompts: Vec<Value>,
    catalog_fingerprint: Option<blake3::Hash>,
    pending_catalog: Option<Vec<Value>>,
    generation: u64,
    transition: Option<Arc<transition::Transition>>,
}

impl ServerEntry {
    fn status(&self) -> ServerStatus {
        ServerStatus {
            id: self.config.id.clone(),
            enabled: self.config.enabled,
            state: self.state.clone(),
            tool_count: self.tools.len(),
            resource_count: self.resources.len(),
            prompt_count: self.prompts.len(),
        }
    }
}

/// Concurrent, deterministic registry for any number of MCP servers.
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
        if servers.len() >= 64 {
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
                tools: Vec::new(),
                resources: Vec::new(),
                prompts: Vec::new(),
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
                sanitize_catalog(refresh_client.list_tools().await?)
            })
            .await?;
        let fingerprint = catalog_fingerprint(&tools);
        let mut servers = self.inner.servers.write().await;
        let entry = servers
            .get_mut(server)
            .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
        if !entry
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
            entry.tools = tools;
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
        if entry.client.is_none() {
            return Err(McpError::NotConnected(server.clone()));
        }
        let Some(tools) = entry.pending_catalog.take() else {
            return Ok(false);
        };
        entry.catalog_fingerprint = Some(catalog_fingerprint(&tools));
        entry.tools = tools;
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

    /// Full definitions for the explicit per-server deferred-loading opt-out.
    #[must_use]
    pub async fn eager_tool_definitions(&self) -> Vec<McpToolDefinition> {
        let servers = self.inner.servers.read().await;
        let mut definitions = Vec::new();
        for (server, entry) in &*servers {
            if !entry.config.enabled
                || entry.config.defer_tools
                || !matches!(entry.state, ServerState::Ready)
            {
                continue;
            }
            for tool in &entry.tools {
                if let Some(definition) = definition(server, tool, &entry.config.tool_capabilities)
                {
                    definitions.push(definition);
                }
            }
        }
        definitions
    }

    /// Exact provider-context fragment used for measured deferred-loading tests.
    pub async fn deferred_prompt(&self) -> Result<String, McpError> {
        serde_json::to_string(&self.deferred_tool_index().await)
            .map_err(|error| McpError::Encoding(error.to_string()))
    }

    /// Full schemas only for matching tools, implementing the built-in `tool_search` behavior.
    pub async fn tool_search(
        &self,
        query: &str,
        server_filter: Option<&McpServerId>,
    ) -> Vec<McpToolDefinition> {
        let query = query.to_ascii_lowercase();
        let servers = self.inner.servers.read().await;
        let mut matches = Vec::new();
        for (server, entry) in &*servers {
            if server_filter.is_some_and(|filter| filter != server)
                || !entry.config.enabled
                || !matches!(entry.state, ServerState::Ready)
            {
                continue;
            }
            for tool in &entry.tools {
                let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !query.is_empty()
                    && !name.to_ascii_lowercase().contains(&query)
                    && !description.to_ascii_lowercase().contains(&query)
                {
                    continue;
                }
                if let Some(definition) = definition(server, tool, &entry.config.tool_capabilities)
                {
                    matches.push(definition);
                }
                if matches.len() >= MAX_SEARCH_RESULTS {
                    return matches;
                }
            }
        }
        matches
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
            if !entry.config.enabled || !matches!(entry.state, ServerState::Ready) {
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
            let value = client.call_tool(&name, arguments).await?;
            manager.cap(&id, "tool result", &value).await
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
            let value = client.read_resource(&uri).await?;
            manager.cap(&id, "resource", &value).await
        })
        .await
    }

    pub async fn get_prompt(
        &self,
        server: &McpServerId,
        name: &str,
        arguments: Value,
    ) -> Result<CappedResponse, McpError> {
        let client = self.client(server).await?;
        let manager = self.clone();
        let id = server.clone();
        let name = name.to_owned();
        self.invoke(server, Arc::clone(&client), async move {
            let value = client.get_prompt(&name, arguments).await?;
            manager.cap(&id, "prompt", &value).await
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
        if !matches!(entry.state, ServerState::Ready) {
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
        value: &Value,
    ) -> Result<CappedResponse, McpError> {
        let encoded = self.inner.encoder.encode(value)?;
        if encoded.len() <= self.inner.limits.response_bytes {
            return Ok(CappedResponse {
                encoded: String::from_utf8_lossy(&encoded).into_owned(),
                format: self.inner.encoder.format().to_owned(),
                truncated: false,
                overflow: None,
            });
        }
        let overflow = self.inner.spool.write(server, operation, &encoded).await?;
        let summary = json!({"truncated":true,"original_bytes":encoded.len(),"overflow":{"id":overflow.id,"bytes":overflow.bytes}});
        let summary = self.inner.encoder.encode(&summary)?;
        if summary.len() > self.inner.limits.response_bytes {
            return Err(McpError::Encoding(
                "overflow reference exceeds MCP response cap".to_owned(),
            ));
        }
        Ok(CappedResponse {
            encoded: String::from_utf8_lossy(&summary).into_owned(),
            format: self.inner.encoder.format().to_owned(),
            truncated: true,
            overflow: Some(overflow),
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
        McpError::Protocol(_) => "MCP protocol operation failed".to_owned(),
        McpError::Encoding(_) => "MCP response encoding failed".to_owned(),
        McpError::Spool(_) => "MCP overflow storage failed".to_owned(),
        McpError::InvalidCommand(_) | McpError::DuplicateServer(_) | McpError::UnknownServer(_) => {
            "MCP configuration is invalid".to_owned()
        }
    }
}

async fn load_catalog(
    client: &dyn McpClient,
) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>), McpError> {
    let (tools, resources, prompts) = tokio::join!(
        client.list_tools(),
        client.list_resources(),
        client.list_prompts()
    );
    Ok((tools?, resources?, prompts?))
}

fn catalog_fingerprint(tools: &[Value]) -> blake3::Hash {
    let bytes = serde_json::to_vec(tools).unwrap_or_default();
    blake3::hash(&bytes)
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

fn sanitize_catalog(values: Vec<Value>) -> Result<Vec<Value>, McpError> {
    if values.len() > MAX_CATALOG_ENTRIES {
        return Err(McpError::Protocol(
            "MCP catalog entry limit exceeded".to_owned(),
        ));
    }
    values
        .into_iter()
        .map(|mut value| {
            let bytes = serde_json::to_vec(&value)
                .map_err(|error| McpError::Protocol(error.to_string()))?;
            if bytes.len() > MAX_CATALOG_ENTRY_BYTES {
                return Err(McpError::Protocol(
                    "MCP catalog entry size limit exceeded".to_owned(),
                ));
            }
            for key in ["name", "description", "uri"] {
                if let Some(text) = value.get(key).and_then(Value::as_str).map(str::to_owned) {
                    let cap = if key == "description" { 512 } else { 256 };
                    value[key] = Value::String(text.chars().take(cap).collect());
                }
            }
            Ok(value)
        })
        .collect()
}

#[cfg(test)]
mod tests;
