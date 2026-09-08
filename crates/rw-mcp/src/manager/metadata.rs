//! Catalog projections acquire destination credit before copying borrowed fields.
use super::{MAX_CATALOG_ENTRIES, MAX_SERVERS, ManagerState, McpManager};
use crate::{
    DeferredTool, McpCatalogEntry, McpError, McpResponse, McpResponseLimits, McpResponseSlot,
    payload_work::Allocation,
};
use rw_types::{McpServerId, json_encoding::JsonWriter};
use serde_json::Value;
use std::sync::Arc;

/// The exact JSON fragment admitted before deferred tools enter provider context.
pub const MAX_DEFERRED_PROMPT_BYTES: usize = 32 * 1024;
const MAX_ENTRIES: usize = MAX_SERVERS * MAX_CATALOG_ENTRIES;

#[derive(Clone, Copy)]
enum Kind {
    Tools,
    Resources,
    Prompts,
}
struct MetadataWork {
    manager: Arc<ManagerState>,
    kind: Kind,
}
#[derive(Clone, Copy)]
struct Fields<'a> {
    server: &'a McpServerId,
    name: &'a str,
    description: &'a str,
    uri: Option<&'a str>,
}
impl Fields<'_> {
    fn bytes(&self) -> Result<usize, McpError> {
        [
            self.server.as_str(),
            self.name,
            self.description,
            self.uri.unwrap_or(""),
        ]
        .into_iter()
        .try_fold(0_usize, |total, text| {
            total.checked_add(text.len()).ok_or_else(exhausted)
        })
    }
}
fn tool(fields: Fields<'_>) -> DeferredTool {
    DeferredTool {
        server: fields.server.clone(),
        name: fields.name.to_owned(),
        description: one_line(fields.description),
    }
}
fn catalog(fields: Fields<'_>) -> McpCatalogEntry {
    McpCatalogEntry {
        server: fields.server.clone(),
        name: fields.name.to_owned(),
        description: fields.description.to_owned(),
        uri: fields.uri.map(str::to_owned),
    }
}

impl MetadataWork {
    fn visit(
        &self,
        servers: &std::collections::BTreeMap<McpServerId, super::ServerEntry>,
        mut visit: impl FnMut(Fields<'_>) -> Result<(), McpError>,
    ) -> Result<(), McpError> {
        for (server, entry) in servers {
            if !entry.ready() || matches!(self.kind, Kind::Tools) && !entry.config.defer_tools {
                continue;
            }
            let values = match self.kind {
                Kind::Tools => &entry.tools,
                Kind::Resources => &entry.resources,
                Kind::Prompts => &entry.prompts,
            };
            for value in values {
                let name = value.get("name").and_then(Value::as_str);
                if matches!(self.kind, Kind::Tools) && name.is_none() {
                    continue;
                }
                visit(Fields {
                    server,
                    name: name.unwrap_or(""),
                    description: value
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    uri: value.get("uri").and_then(Value::as_str),
                })?;
            }
        }
        Ok(())
    }
    fn run<T>(&self, project: impl Fn(Fields<'_>) -> T) -> Result<McpResponse<Vec<T>>, McpError> {
        let servers = self.manager.servers.blocking_read();
        let mut count = 0_usize;
        let mut bytes = 4096_usize;
        self.visit(&servers, |fields| {
            count = count
                .checked_add(1)
                .filter(|count| *count <= MAX_ENTRIES)
                .ok_or_else(exhausted)?;
            bytes = fields
                .bytes()?
                .checked_mul(2)
                .and_then(|fields| fields.checked_add(std::mem::size_of::<T>()))
                .and_then(|entry| bytes.checked_add(entry))
                .ok_or_else(exhausted)?;
            Ok(())
        })?;
        let retained = Allocation::new(bytes)?;
        let mut output = McpResponse::wire(Vec::with_capacity(count), vec![Arc::new(retained)]);
        self.visit(&servers, |fields| {
            output.value.push(project(fields));
            Ok(())
        })?;
        Ok(output)
    }
}
impl McpManager {
    /// Borrowed introspection with no schemas; source and destination are charged
    /// simultaneously until the physical projection finishes.
    pub async fn deferred_tool_index(&self) -> Result<McpResponse<Vec<DeferredTool>>, McpError> {
        let work = MetadataWork {
            manager: Arc::clone(&self.inner),
            kind: Kind::Tools,
        };
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run(tool))
            .await
            .map_err(|_| exhausted())?
    }
    /// Exact bounded provider-context fragment used by deferred-loading acceptance.
    pub async fn deferred_prompt(&self) -> Result<McpResponse<String>, McpError> {
        self.deferred_tool_index()
            .await?
            .project(
                McpResponseSlot::new(McpResponseLimits::new(
                    MAX_DEFERRED_PROMPT_BYTES * 2 + 4096,
                )?)?,
                |index| encode_deferred(index),
            )
            .await
    }
    pub async fn resources(&self) -> Result<McpResponse<Vec<McpCatalogEntry>>, McpError> {
        self.catalog_metadata(Kind::Resources).await
    }
    pub async fn prompts(&self) -> Result<McpResponse<Vec<McpCatalogEntry>>, McpError> {
        self.catalog_metadata(Kind::Prompts).await
    }
    async fn catalog_metadata(
        &self,
        kind: Kind,
    ) -> Result<McpResponse<Vec<McpCatalogEntry>>, McpError> {
        let work = MetadataWork {
            manager: Arc::clone(&self.inner),
            kind,
        };
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run(catalog))
            .await
            .map_err(|_| exhausted())?
    }
}
fn encode_deferred(index: &[DeferredTool]) -> Result<String, McpError> {
    let mut bytes = Vec::new();
    JsonWriter::buffer(&mut bytes, MAX_DEFERRED_PROMPT_BYTES, 4096)
        .map_err(|error| McpError::Encoding(error.to_string()))?
        .serialize(index)
        .map_err(|error| McpError::Encoding(error.to_string()))?;
    String::from_utf8(bytes).map_err(|_| exhausted())
}
fn one_line(value: &str) -> String {
    let mut output = String::with_capacity(value.len().min(640));
    let mut chars = value
        .split_whitespace()
        .flat_map(|word| word.chars().chain(std::iter::once(' ')))
        .take(161)
        .peekable();
    for _ in 0..160 {
        let Some(character) = chars.next() else {
            break;
        };
        // The injected separator after the final word is not part of the index.
        if character == ' ' && chars.peek().is_none() {
            break;
        }
        output.push(character);
    }
    output
}
fn exhausted() -> McpError {
    McpError::Protocol("MCP metadata projection allocation exceeded".into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    #[test]
    fn deferred_json_preserves_exact_bytes_and_refuses_oversized_catalogs() {
        let entry = DeferredTool {
            server: McpServerId::new("fixture").expect("id"),
            name: "tool".into(),
            description: "text <&> 世界".into(),
        };
        let index = vec![entry.clone(), entry];
        assert_eq!(
            encode_deferred(&index).expect("bounded"),
            serde_json::to_string(&index).expect("reference")
        );
        let oversized = vec![
            DeferredTool {
                server: McpServerId::new("fixture").expect("id"),
                name: "tool".into(),
                description: "x".repeat(160)
            };
            MAX_CATALOG_ENTRIES
        ];
        assert!(encode_deferred(&oversized).is_err());
    }
    #[test]
    fn one_line_streams_whitespace_and_preserves_unicode_prefix() {
        for text in [
            "",
            " \t \n",
            "  hello  世界\nthere ",
            "a ",
            "a",
            &"🦀".repeat(1000),
            &"a ".repeat(1000),
        ] {
            let expected = text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(160)
                .collect::<String>();
            assert_eq!(one_line(text), expected);
        }
    }
}
