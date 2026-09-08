//! Schema copies are admitted before construction and travel with their result.
use super::{MAX_SEARCH_RESULTS, ManagerState, McpManager, definition};
use crate::{McpError, McpResponse, McpToolDefinition, ServerState, payload_work::Allocation};
use rw_types::{McpServerId, allocation::PrepareAllocation};
use std::sync::Arc;

struct SearchInput {
    query: String,
    server: Option<McpServerId>,
    _retained: Allocation,
}
struct SearchWork {
    input: SearchInput,
    manager: Arc<ManagerState>,
}
struct SearchOutput {
    values: Vec<McpToolDefinition>,
    retained: Allocation,
    bytes: usize,
}
impl SearchOutput {
    fn new() -> Result<Self, McpError> {
        let bytes = MAX_SEARCH_RESULTS * std::mem::size_of::<McpToolDefinition>() + 4096;
        let retained = Allocation::new(bytes)?;
        Ok(Self {
            values: Vec::with_capacity(MAX_SEARCH_RESULTS),
            retained,
            bytes,
        })
    }
    fn admit(&mut self, tool: &serde_json::Value, server: &McpServerId) -> Result<(), McpError> {
        let bytes = tool
            .prepared_bytes()
            .and_then(|bytes| bytes.checked_mul(2))
            .and_then(|bytes| bytes.checked_add(server.as_str().len().saturating_mul(2)))
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or_else(exhausted)?;
        let total = self.bytes.checked_add(bytes).ok_or_else(exhausted)?;
        self.retained.resize(total)?;
        self.bytes = total;
        Ok(())
    }
    fn finish(self) -> McpResponse<Vec<McpToolDefinition>> {
        McpResponse::wire(self.values, vec![Arc::new(self.retained)])
    }
}
impl SearchWork {
    fn run(self) -> Result<McpResponse<Vec<McpToolDefinition>>, McpError> {
        let mut output = SearchOutput::new()?;
        let servers = self.manager.servers.blocking_read();
        for (server, entry) in &*servers {
            if self
                .input
                .server
                .as_ref()
                .is_some_and(|filter| filter != server)
                || !entry.config.enabled
                || !matches!(entry.state, ServerState::Ready)
                || !entry.catalog_valid()
            {
                continue;
            }
            for tool in &entry.tools {
                let name = tool
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let description = tool
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !contains(name, &self.input.query) && !contains(description, &self.input.query) {
                    continue;
                }
                output.admit(tool, server)?;
                if let Some(value) = definition(server, tool, &entry.config.tool_capabilities) {
                    output.values.push(value);
                }
                if output.values.len() == MAX_SEARCH_RESULTS {
                    return Ok(output.finish());
                }
            }
        }
        Ok(output.finish())
    }
}
fn contains(text: &str, query: &str) -> bool {
    query.is_empty()
        || text
            .as_bytes()
            .windows(query.len())
            .any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
}
fn exhausted() -> McpError {
    McpError::Protocol("MCP schema result allocation exhausted".into())
}

impl McpManager {
    /// Return matching schemas with admission held through their final consumer.
    pub async fn tool_search(
        &self,
        query: &str,
        server: Option<&McpServerId>,
    ) -> Result<McpResponse<Vec<McpToolDefinition>>, McpError> {
        let bytes = query
            .len()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or_else(exhausted)?;
        let retained = Allocation::new(bytes)?;
        let work = SearchWork {
            input: SearchInput {
                query: query.to_owned(),
                server: server.cloned(),
                _retained: retained,
            },
            manager: Arc::clone(&self.inner),
        };
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
            .await
            .map_err(|_| exhausted())?
    }
}
