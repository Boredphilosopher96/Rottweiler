use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Value, json};
use tokio::{sync::RwLock, task::JoinSet};

use crate::{
    CappedResponse, DeferredTool, McpCatalogEntry, McpClient, McpConnector, McpError, McpLimits,
    McpServerConfig, McpToolDefinition, OverflowSpool, ServerId, ServerState, ServerStatus,
};
use rw_tools::CapabilityManifest;

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
pub struct McpManager {
    connector: Arc<dyn McpConnector>,
    spool: Arc<dyn OverflowSpool>,
    encoder: Arc<dyn StructuredResponseEncoder>,
    limits: McpLimits,
    servers: RwLock<BTreeMap<ServerId, ServerEntry>>,
    tool_capabilities: std::sync::RwLock<BTreeMap<ServerId, crate::McpToolCapabilityOverrides>>,
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
            connector,
            spool,
            encoder,
            limits,
            servers: RwLock::new(BTreeMap::new()),
            tool_capabilities: std::sync::RwLock::new(BTreeMap::new()),
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
        let mut servers = self.servers.write().await;
        if servers.contains_key(&config.id) {
            return Err(McpError::DuplicateServer(config.id));
        }
        self.tool_capabilities
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
            },
        );
        Ok(())
    }

    /// Removes a server that has not been enabled. This is deliberately
    /// narrower than a general unregister operation: callers use it to roll
    /// back a live registration when durable configuration persistence fails.
    pub async fn unregister_disabled(&self, server: &ServerId) -> Result<(), McpError> {
        let mut servers = self.servers.write().await;
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
        self.tool_capabilities
            .write()
            .map_err(|_| McpError::Policy("MCP capability policy lock was poisoned".to_owned()))?
            .remove(server);
        Ok(())
    }

    /// Resolves permission effects without awaiting so core can classify an
    /// invocation before the permission gate runs. Unknown or poisoned state
    /// remains network + execute.
    #[must_use]
    pub fn tool_capabilities(&self, server: &ServerId, tool: &str) -> CapabilityManifest {
        self.tool_capabilities
            .read()
            .ok()
            .and_then(|policies| policies.get(server).map(|policy| policy.resolve(tool)))
            .unwrap_or_else(McpToolDefinition::restrictive_capabilities)
    }

    /// Connects all enabled servers concurrently and returns failures without hiding successes.
    #[allow(clippy::too_many_lines)]
    pub async fn connect_all(&self) -> Vec<(ServerId, Result<(), McpError>)> {
        let configs = self
            .servers
            .read()
            .await
            .values()
            .filter(|entry| entry.config.enabled && matches!(entry.state, ServerState::Connecting))
            .map(|entry| (entry.config.clone(), entry.generation))
            .collect::<Vec<_>>();
        let mut jobs = JoinSet::new();
        for (config, generation) in configs {
            let connector = Arc::clone(&self.connector);
            let timeout = self.limits.request_timeout;
            jobs.spawn(async move {
                let id = config.id.clone();
                let result = tokio::time::timeout(timeout, connector.connect(&config))
                    .await
                    .map_err(|_| McpError::Protocol("MCP connect timed out".to_owned()))
                    .and_then(std::convert::identity);
                (id, generation, result)
            });
        }
        let mut results = Vec::new();
        while let Some(joined) = jobs.join_next().await {
            let Ok((id, generation, connected)) = joined else {
                results.push((
                    ServerId("internal-task".to_owned()),
                    Err(McpError::Protocol("MCP connection task failed".to_owned())),
                ));
                continue;
            };
            match connected {
                Ok(client) => {
                    let catalog =
                        tokio::time::timeout(self.limits.request_timeout, load_catalog(&*client))
                            .await
                            .map_err(|_| {
                                McpError::Protocol("MCP catalog request timed out".to_owned())
                            })
                            .and_then(std::convert::identity);
                    match catalog {
                        Ok((tools, resources, prompts)) => {
                            let tools = match sanitize_catalog(tools) {
                                Ok(value) => value,
                                Err(error) => {
                                    let _ = client.close(self.limits.shutdown_timeout).await;
                                    results.push((id, Err(error)));
                                    continue;
                                }
                            };
                            let resources = match sanitize_catalog(resources) {
                                Ok(value) => value,
                                Err(error) => {
                                    let _ = client.close(self.limits.shutdown_timeout).await;
                                    results.push((id, Err(error)));
                                    continue;
                                }
                            };
                            let prompts = match sanitize_catalog(prompts) {
                                Ok(value) => value,
                                Err(error) => {
                                    let _ = client.close(self.limits.shutdown_timeout).await;
                                    results.push((id, Err(error)));
                                    continue;
                                }
                            };
                            let fingerprint = catalog_fingerprint(&tools);
                            let accepted = {
                                let mut servers = self.servers.write().await;
                                if let Some(entry) = servers.get_mut(&id) {
                                    if entry.config.enabled
                                        && entry.generation == generation
                                        && matches!(entry.state, ServerState::Connecting)
                                    {
                                        entry.client = Some(Arc::clone(&client));
                                        if entry.catalog_fingerprint.is_some()
                                            && entry.catalog_fingerprint != Some(fingerprint)
                                        {
                                            entry.pending_catalog = Some(tools);
                                        } else {
                                            entry.tools = tools;
                                            entry.catalog_fingerprint = Some(fingerprint);
                                        }
                                        entry.resources = resources;
                                        entry.prompts = prompts;
                                        entry.state = if entry.pending_catalog.is_some() {
                                            ServerState::ApprovalRequired
                                        } else {
                                            ServerState::Ready
                                        };
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            };
                            if accepted {
                                results.push((id, Ok(())));
                            } else {
                                let _ = client.close(self.limits.shutdown_timeout).await;
                                results.push((
                                    id,
                                    Err(McpError::Disabled(ServerId(
                                        "stale-connection".to_owned(),
                                    ))),
                                ));
                            }
                        }
                        Err(error) => {
                            let message = status_message(&error);
                            let _ = client.close(self.limits.shutdown_timeout).await;
                            if let Some(entry) = self.servers.write().await.get_mut(&id)
                                && entry.generation == generation
                                && entry.config.enabled
                            {
                                entry.state = ServerState::Failed { message };
                            }
                            results.push((id, Err(error)));
                        }
                    }
                }
                Err(error) => {
                    if let Some(entry) = self.servers.write().await.get_mut(&id)
                        && entry.generation == generation
                        && entry.config.enabled
                    {
                        entry.state = ServerState::Failed {
                            message: status_message(&error),
                        };
                    }
                    results.push((id, Err(error)));
                }
            }
        }
        for entry in self.servers.write().await.values_mut() {
            if entry.config.enabled && matches!(entry.state, ServerState::Connecting) {
                entry.state = ServerState::Failed {
                    message: "MCP connection task did not complete".to_owned(),
                };
            }
        }
        results.sort_by(|left, right| left.0.cmp(&right.0));
        results
    }

    /// Re-lists tools. Changed schemas stay pending until `approve_changes` is true.
    pub async fn refresh_tools(
        &self,
        server: &ServerId,
        approve_changes: bool,
    ) -> Result<bool, McpError> {
        let client = self.client(server).await?;
        let tools = sanitize_catalog(
            tokio::time::timeout(self.limits.request_timeout, client.list_tools())
                .await
                .map_err(|_| McpError::Protocol("MCP tool refresh timed out".to_owned()))??,
        )?;
        let fingerprint = catalog_fingerprint(&tools);
        let mut servers = self.servers.write().await;
        let entry = servers
            .get_mut(server)
            .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
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

    pub async fn approve_pending_tools(&self, server: &ServerId) -> Result<bool, McpError> {
        let mut servers = self.servers.write().await;
        let entry = servers
            .get_mut(server)
            .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
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
        self.servers
            .read()
            .await
            .values()
            .map(ServerEntry::status)
            .collect()
    }

    /// Name + one-line description only: no input schemas or annotations.
    #[must_use]
    pub async fn deferred_tool_index(&self) -> Vec<DeferredTool> {
        let servers = self.servers.read().await;
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
        let servers = self.servers.read().await;
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
        server_filter: Option<&ServerId>,
    ) -> Vec<McpToolDefinition> {
        let query = query.to_ascii_lowercase();
        let servers = self.servers.read().await;
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
        let servers = self.servers.read().await;
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
        server: &ServerId,
        name: &str,
        arguments: Value,
    ) -> Result<CappedResponse, McpError> {
        let approved = self.servers.read().await.get(server).is_some_and(|entry| {
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
        let value = tokio::time::timeout(
            self.limits.request_timeout,
            client.call_tool(name, arguments),
        )
        .await
        .map_err(|_| McpError::Protocol("MCP tool request timed out".to_owned()))??;
        self.cap(server, "tool result", &value).await
    }

    pub async fn read_resource(
        &self,
        server: &ServerId,
        uri: &str,
    ) -> Result<CappedResponse, McpError> {
        let client = self.client(server).await?;
        let value = tokio::time::timeout(self.limits.request_timeout, client.read_resource(uri))
            .await
            .map_err(|_| McpError::Protocol("MCP resource request timed out".to_owned()))??;
        self.cap(server, "resource", &value).await
    }

    pub async fn get_prompt(
        &self,
        server: &ServerId,
        name: &str,
        arguments: Value,
    ) -> Result<CappedResponse, McpError> {
        let client = self.client(server).await?;
        let value = tokio::time::timeout(
            self.limits.request_timeout,
            client.get_prompt(name, arguments),
        )
        .await
        .map_err(|_| McpError::Protocol("MCP prompt request timed out".to_owned()))??;
        self.cap(server, "prompt", &value).await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn set_enabled(&self, server: &ServerId, enabled: bool) -> Result<(), McpError> {
        if enabled {
            let (config, generation) = {
                let mut servers = self.servers.write().await;
                let entry = servers
                    .get_mut(server)
                    .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
                if matches!(entry.state, ServerState::Ready) {
                    entry.config.enabled = true;
                    return Ok(());
                }
                entry.config.enabled = true;
                entry.state = ServerState::Connecting;
                entry.generation = entry.generation.wrapping_add(1);
                (entry.config.clone(), entry.generation)
            };
            return self.connect_generation(server, config, generation).await;
        }
        let client = {
            let mut servers = self.servers.write().await;
            let entry = servers
                .get_mut(server)
                .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
            entry.config.enabled = false;
            entry.generation = entry.generation.wrapping_add(1);
            entry.state = ServerState::Stopping;
            entry.client.take()
        };
        let close_result = if let Some(client) = client {
            client.close(self.limits.shutdown_timeout).await
        } else {
            Ok(())
        };
        if let Some(entry) = self.servers.write().await.get_mut(server) {
            entry.state = close_result.as_ref().map_or_else(
                |error| ServerState::Failed {
                    message: status_message(error),
                },
                |()| ServerState::Disabled,
            );
        }
        close_result
    }

    /// Atomically transition a failed server to a new connection attempt.
    ///
    /// Returns `false` without changing `Ready`, `ApprovalRequired`, `Disabled`,
    /// or in-flight state, so repeated durable approval cannot replace a live
    /// client or undo an explicit disable.
    pub async fn reconnect_if_failed(&self, server: &ServerId) -> Result<bool, McpError> {
        let prepared = {
            let mut servers = self.servers.write().await;
            let entry = servers
                .get_mut(server)
                .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
            if !entry.config.enabled || !matches!(entry.state, ServerState::Failed { .. }) {
                return Ok(false);
            }
            entry.state = ServerState::Connecting;
            entry.generation = entry.generation.wrapping_add(1);
            (entry.config.clone(), entry.generation)
        };
        self.connect_generation(server, prepared.0, prepared.1)
            .await?;
        Ok(true)
    }

    async fn connect_generation(
        &self,
        server: &ServerId,
        config: McpServerConfig,
        generation: u64,
    ) -> Result<(), McpError> {
        let connected =
            tokio::time::timeout(self.limits.request_timeout, self.connector.connect(&config))
                .await;
        let client = match connected {
            Ok(Ok(client)) => client,
            Ok(Err(error)) => {
                self.fail_generation(server, generation, &error).await;
                return Err(error);
            }
            Err(_) => {
                let error = McpError::Protocol("MCP connect timed out".to_owned());
                self.fail_generation(server, generation, &error).await;
                return Err(error);
            }
        };
        let catalog =
            tokio::time::timeout(self.limits.request_timeout, load_catalog(&*client)).await;
        let (tools, resources, prompts) = match catalog {
            Ok(Ok(values)) => values,
            Ok(Err(error)) => {
                let _ = client.close(self.limits.shutdown_timeout).await;
                self.fail_generation(server, generation, &error).await;
                return Err(error);
            }
            Err(_) => {
                let error = McpError::Protocol("MCP catalog request timed out".to_owned());
                let _ = client.close(self.limits.shutdown_timeout).await;
                self.fail_generation(server, generation, &error).await;
                return Err(error);
            }
        };
        let sanitized = sanitize_catalog(tools).and_then(|tools| {
            Ok((
                tools,
                sanitize_catalog(resources)?,
                sanitize_catalog(prompts)?,
            ))
        });
        let (tools, resources, prompts) = match sanitized {
            Ok(values) => values,
            Err(error) => {
                let _ = client.close(self.limits.shutdown_timeout).await;
                self.fail_generation(server, generation, &error).await;
                return Err(error);
            }
        };
        let mut servers = self.servers.write().await;
        let entry = servers
            .get_mut(server)
            .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
        if !entry.config.enabled || entry.generation != generation {
            drop(servers);
            let _ = client.close(self.limits.shutdown_timeout).await;
            return Err(McpError::Protocol(
                "stale MCP connection was discarded".to_owned(),
            ));
        }
        let fingerprint = catalog_fingerprint(&tools);
        if entry.catalog_fingerprint.is_some() && entry.catalog_fingerprint != Some(fingerprint) {
            entry.pending_catalog = Some(tools);
        } else {
            entry.catalog_fingerprint = Some(fingerprint);
            entry.tools = tools;
        }
        entry.resources = resources;
        entry.prompts = prompts;
        entry.client = Some(client);
        entry.state = if entry.pending_catalog.is_some() {
            ServerState::ApprovalRequired
        } else {
            ServerState::Ready
        };
        Ok(())
    }

    pub async fn shutdown(&self) -> Vec<(ServerId, Result<(), McpError>)> {
        let clients = {
            let mut servers = self.servers.write().await;
            servers
                .iter_mut()
                .filter_map(|(id, entry)| {
                    entry.config.enabled = false;
                    entry.generation = entry.generation.wrapping_add(1);
                    entry.state = if entry.client.is_some() {
                        ServerState::Stopping
                    } else {
                        ServerState::Disabled
                    };
                    entry.client.take().map(|client| (id.clone(), client))
                })
                .collect::<Vec<_>>()
        };
        let mut jobs = JoinSet::new();
        for (id, client) in clients {
            let timeout = self.limits.shutdown_timeout;
            jobs.spawn(async move { (id, client.close(timeout).await) });
        }
        let mut results = Vec::new();
        while let Some(joined) = jobs.join_next().await {
            let Ok((id, result)) = joined else {
                results.push((
                    ServerId("internal-shutdown-task".to_owned()),
                    Err(McpError::Protocol("MCP shutdown task failed".to_owned())),
                ));
                continue;
            };
            if let Some(entry) = self.servers.write().await.get_mut(&id) {
                entry.state = result.as_ref().map_or_else(
                    |error| ServerState::Failed {
                        message: status_message(error),
                    },
                    |()| ServerState::Disabled,
                );
            }
            results.push((id, result));
        }
        results.sort_by(|left, right| left.0.cmp(&right.0));
        results
    }

    async fn client(&self, server: &ServerId) -> Result<Arc<dyn McpClient>, McpError> {
        let servers = self.servers.read().await;
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

    async fn fail_generation(&self, server: &ServerId, generation: u64, error: &McpError) {
        if let Some(entry) = self.servers.write().await.get_mut(server)
            && entry.config.enabled
            && entry.generation == generation
        {
            entry.state = ServerState::Failed {
                message: status_message(error),
            };
        }
    }

    async fn cap(
        &self,
        server: &ServerId,
        operation: &str,
        value: &Value,
    ) -> Result<CappedResponse, McpError> {
        let encoded = self.encoder.encode(value)?;
        if encoded.len() <= self.limits.response_bytes {
            return Ok(CappedResponse {
                encoded: String::from_utf8_lossy(&encoded).into_owned(),
                format: self.encoder.format().to_owned(),
                truncated: false,
                overflow: None,
            });
        }
        let overflow = self.spool.write(server, operation, &encoded).await?;
        let summary = json!({"truncated":true,"original_bytes":encoded.len(),"overflow":{"id":overflow.id,"bytes":overflow.bytes}});
        let summary = self.encoder.encode(&summary)?;
        if summary.len() > self.limits.response_bytes {
            return Err(McpError::Encoding(
                "overflow reference exceeds MCP response cap".to_owned(),
            ));
        }
        Ok(CappedResponse {
            encoded: String::from_utf8_lossy(&summary).into_owned(),
            format: self.encoder.format().to_owned(),
            truncated: true,
            overflow: Some(overflow),
        })
    }
}

fn status_message(error: &McpError) -> String {
    match error {
        McpError::PendingLogin { .. } => "MCP login is required".to_owned(),
        McpError::Disabled(_) => "MCP server is disabled".to_owned(),
        McpError::NotConnected(_) => "MCP server is not connected".to_owned(),
        McpError::ShutdownTimeout(_) => "MCP server shutdown timed out".to_owned(),
        McpError::Policy(_) => "MCP transport policy rejected the connection".to_owned(),
        McpError::Protocol(_) => "MCP protocol operation failed".to_owned(),
        McpError::Encoding(_) => "MCP response encoding failed".to_owned(),
        McpError::Spool(_) => "MCP overflow storage failed".to_owned(),
        McpError::InvalidServerId(_)
        | McpError::InvalidCommand(_)
        | McpError::DuplicateServer(_)
        | McpError::UnknownServer(_) => "MCP configuration is invalid".to_owned(),
    }
}

async fn load_catalog(
    client: &dyn McpClient,
) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>), McpError> {
    let (tools, resources, prompts) = tokio::try_join!(
        client.list_tools(),
        client.list_resources(),
        client.list_prompts()
    )?;
    Ok((tools, resources, prompts))
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
    server: &ServerId,
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
mod tests {
    #![allow(clippy::expect_used)]

    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use tokio::sync::{Mutex, Notify};

    use super::*;
    use crate::{McpStdioSandboxPolicy, McpTransportConfig, OverflowReference};

    struct MockClient {
        schema_version: Arc<Mutex<u8>>,
        closed: AtomicBool,
        fail_close: bool,
    }

    #[async_trait]
    impl McpClient for MockClient {
        async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
            let version = *self.schema_version.lock().await;
            Ok(vec![
                json!({"name":"lookup","description":"Look up records\nwithout loading this large schema.","inputSchema":{"type":"object","properties":{"version":{"const":version},"query":{"type":"string"}}}}),
            ])
        }
        async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
            Ok(vec![
                json!({"name":"guide","uri":"memory://guide","description":"Guide"}),
            ])
        }
        async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
            Ok(vec![
                json!({"name":"review","description":"Review a change"}),
            ])
        }
        async fn call_tool(&self, _name: &str, arguments: Value) -> Result<Value, McpError> {
            Ok(arguments)
        }
        async fn read_resource(&self, uri: &str) -> Result<Value, McpError> {
            Ok(json!({"uri":uri,"text":"resource"}))
        }
        async fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
            Ok(json!({"name":name,"arguments":arguments}))
        }
        async fn close(&self, _timeout: Duration) -> Result<(), McpError> {
            self.closed.store(true, Ordering::Release);
            if self.fail_close {
                Err(McpError::Protocol("fixture close failed".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    struct MockConnector {
        clients: Mutex<BTreeMap<ServerId, Arc<MockClient>>>,
    }

    struct BlockingConnector {
        client: Arc<MockClient>,
        started: Notify,
        proceed: Notify,
    }

    #[async_trait]
    impl McpConnector for BlockingConnector {
        async fn connect(&self, _config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
            self.started.notify_one();
            self.proceed.notified().await;
            Ok(self.client.clone())
        }
    }
    #[async_trait]
    impl McpConnector for MockConnector {
        async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
            self.clients
                .lock()
                .await
                .get(&config.id)
                .cloned()
                .map(|client| client as Arc<dyn McpClient>)
                .ok_or_else(|| McpError::UnknownServer(config.id.clone()))
        }
    }

    #[derive(Default)]
    struct MemorySpool {
        values: Mutex<Vec<Vec<u8>>>,
    }
    #[async_trait]
    impl OverflowSpool for MemorySpool {
        async fn write(
            &self,
            server: &ServerId,
            _operation: &str,
            bytes: &[u8],
        ) -> Result<OverflowReference, McpError> {
            self.values.lock().await.push(bytes.to_vec());
            Ok(OverflowReference {
                id: format!("opaque-{}", server.0),
                bytes: bytes.len(),
            })
        }
        async fn read(&self, reference: &OverflowReference) -> Result<Vec<u8>, McpError> {
            self.values
                .lock()
                .await
                .iter()
                .find(|value| value.len() == reference.bytes)
                .cloned()
                .ok_or_else(|| McpError::Spool("missing".to_owned()))
        }
        async fn remove(&self, _reference: &OverflowReference) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn five_servers_stay_deferred_and_support_full_catalog_and_calls() {
        let mut clients = BTreeMap::new();
        for index in 0..5 {
            let id = ServerId::new(format!("server-{index}")).expect("id");
            clients.insert(
                id,
                Arc::new(MockClient {
                    schema_version: Arc::new(Mutex::new(1)),
                    closed: AtomicBool::new(false),
                    fail_close: false,
                }),
            );
        }
        let connector = Arc::new(MockConnector {
            clients: Mutex::new(clients),
        });
        let spool = Arc::new(MemorySpool::default());
        let manager = McpManager::new(
            connector,
            spool,
            Arc::new(CompactJsonEncoder),
            McpLimits {
                response_bytes: 128,
                request_timeout: Duration::from_secs(1),
                shutdown_timeout: Duration::from_secs(1),
            },
        );
        for index in 0..5 {
            manager
                .register(McpServerConfig {
                    id: ServerId::new(format!("server-{index}")).expect("id"),
                    transport: McpTransportConfig::Stdio {
                        executable: "fixture".into(),
                        args: Vec::new(),
                        working_directory: None,
                        environment: Vec::new(),
                        sandbox: McpStdioSandboxPolicy::default(),
                    },
                    enabled: true,
                    defer_tools: true,
                    tool_capabilities: crate::McpToolCapabilityOverrides::default(),
                })
                .await
                .expect("register");
        }
        assert!(
            manager
                .connect_all()
                .await
                .into_iter()
                .all(|(_, result)| result.is_ok())
        );
        let prompt = manager.deferred_prompt().await.expect("prompt");
        let tokenizer = tiktoken_rs::cl100k_base().expect("tokenizer");
        assert!(tokenizer.encode_with_special_tokens(&prompt).len() < 2_000);
        let index_json = serde_json::to_value(manager.deferred_tool_index().await).expect("index");
        assert!(index_json.to_string().find("inputSchema").is_none());
        let definitions = manager.tool_search("look", None).await;
        assert_eq!(definitions.len(), 5);
        assert_eq!(definitions[0].capabilities.capabilities().len(), 2);
        assert_eq!(manager.resources().await.len(), 5);
        assert_eq!(manager.prompts().await.len(), 5);
        let server = ServerId::new("server-0").expect("id");
        assert!(
            !manager
                .call_tool(&server, "lookup", json!({"small":true}))
                .await
                .expect("call")
                .truncated
        );
        assert_eq!(
            manager
                .read_resource(&server, "memory://guide")
                .await
                .expect("resource")
                .format,
            "json"
        );
        assert!(
            manager
                .get_prompt(&server, "review", json!({"large":"x".repeat(512)}))
                .await
                .expect("prompt")
                .truncated
        );
        assert!(
            manager
                .shutdown()
                .await
                .into_iter()
                .all(|(_, result)| result.is_ok())
        );
    }

    #[tokio::test]
    async fn changed_schema_stays_inactive_until_approval() {
        let id = ServerId::new("mutable").expect("id");
        let schema_version = Arc::new(Mutex::new(1));
        let client = Arc::new(MockClient {
            schema_version: Arc::clone(&schema_version),
            closed: AtomicBool::new(false),
            fail_close: false,
        });
        let connector = Arc::new(MockConnector {
            clients: Mutex::new(BTreeMap::from([(id.clone(), client)])),
        });
        let manager = McpManager::new(
            connector,
            Arc::new(MemorySpool::default()),
            Arc::new(CompactJsonEncoder),
            McpLimits::default(),
        );
        manager
            .register(McpServerConfig {
                id: id.clone(),
                transport: McpTransportConfig::Stdio {
                    executable: "fixture".into(),
                    args: vec![],
                    working_directory: None,
                    environment: vec![],
                    sandbox: McpStdioSandboxPolicy::default(),
                },
                enabled: true,
                defer_tools: true,
                tool_capabilities: crate::McpToolCapabilityOverrides::default(),
            })
            .await
            .expect("register");
        assert!(manager.connect_all().await[0].1.is_ok());
        *schema_version.lock().await = 2;
        assert!(manager.refresh_tools(&id, false).await.expect("refresh"));
        assert!(matches!(
            manager.call_tool(&id, "lookup", json!({})).await,
            Err(McpError::NotConnected(_))
        ));
        assert!(manager.tool_search("lookup", Some(&id)).await.is_empty());
        assert!(manager.approve_pending_tools(&id).await.expect("approve"));
        assert_eq!(
            manager.tool_search("lookup", Some(&id)).await[0].input_schema["properties"]["version"]
                ["const"],
            2
        );
        manager.set_enabled(&id, false).await.expect("disable");
        *schema_version.lock().await = 3;
        manager.set_enabled(&id, true).await.expect("re-enable");
        assert!(matches!(
            manager.call_tool(&id, "lookup", json!({})).await,
            Err(McpError::NotConnected(_))
        ));
        assert!(manager.tool_search("lookup", Some(&id)).await.is_empty());
        assert!(
            manager
                .approve_pending_tools(&id)
                .await
                .expect("approve reconnect")
        );
        assert_eq!(
            manager.tool_search("lookup", Some(&id)).await[0].input_schema["properties"]["version"]
                ["const"],
            3
        );
    }

    #[tokio::test]
    async fn failed_close_does_not_make_an_explicitly_disabled_server_reconnectable() {
        let id = ServerId::new("close-failure").expect("id");
        let client = Arc::new(MockClient {
            schema_version: Arc::new(Mutex::new(1)),
            closed: AtomicBool::new(false),
            fail_close: true,
        });
        let connector = Arc::new(MockConnector {
            clients: Mutex::new(BTreeMap::from([(id.clone(), client)])),
        });
        let manager = McpManager::new(
            connector,
            Arc::new(MemorySpool::default()),
            Arc::new(CompactJsonEncoder),
            McpLimits::default(),
        );
        manager
            .register(McpServerConfig {
                id: id.clone(),
                transport: McpTransportConfig::Stdio {
                    executable: "fixture".into(),
                    args: vec![],
                    working_directory: None,
                    environment: vec![],
                    sandbox: McpStdioSandboxPolicy::default(),
                },
                enabled: true,
                defer_tools: true,
                tool_capabilities: crate::McpToolCapabilityOverrides::default(),
            })
            .await
            .expect("register");
        assert!(manager.connect_all().await[0].1.is_ok());
        manager
            .set_enabled(&id, false)
            .await
            .expect_err("fixture close fails");
        assert!(!manager.reconnect_if_failed(&id).await.expect("retry gate"));
        let status = manager.statuses().await;
        assert!(!status[0].enabled);
        assert!(matches!(status[0].state, ServerState::Failed { .. }));
    }

    #[tokio::test]
    async fn disable_during_connect_cannot_resurrect_stale_generation() {
        let id = ServerId::new("racing").expect("id");
        let connector = Arc::new(BlockingConnector {
            client: Arc::new(MockClient {
                schema_version: Arc::new(Mutex::new(1)),
                closed: AtomicBool::new(false),
                fail_close: false,
            }),
            started: Notify::new(),
            proceed: Notify::new(),
        });
        let manager = Arc::new(McpManager::new(
            connector.clone(),
            Arc::new(MemorySpool::default()),
            Arc::new(CompactJsonEncoder),
            McpLimits::default(),
        ));
        manager
            .register(McpServerConfig {
                id: id.clone(),
                transport: McpTransportConfig::Stdio {
                    executable: "fixture".into(),
                    args: vec![],
                    working_directory: None,
                    environment: vec![],
                    sandbox: McpStdioSandboxPolicy::default(),
                },
                enabled: true,
                defer_tools: true,
                tool_capabilities: crate::McpToolCapabilityOverrides::default(),
            })
            .await
            .expect("register");
        let running = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.connect_all().await })
        };
        connector.started.notified().await;
        manager.set_enabled(&id, false).await.expect("disable");
        connector.proceed.notify_one();
        let _ = running.await.expect("join");
        let status = manager.statuses().await.remove(0);
        assert!(!status.enabled);
        assert_eq!(status.state, ServerState::Disabled);
        assert_eq!(status.tool_count, 0);
    }
}
