//! Search schemas stay borrowed until bounded output construction has admission.
use super::{ToolSearchInput, mcp_tool_error, presentation, untrusted_result};
use rw_mcp::{McpManager, McpResponse, McpResponseLimits, McpResponseSlot, McpToolDefinition};
use rw_tools::{McpToolPolicy, ToolContext, ToolError, ToolResult, ToolResultPayloads};
use rw_types::{
    json_encoding::JsonWriter,
    json_structure::{JsonStructureLimits, preflight_json},
};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

const MAX_SCHEMAS_BYTES: usize = 192 * 1024;
const MAX_OUTPUT_BYTES: usize = MAX_SCHEMAS_BYTES + 128;
const FIXED_WORK_BYTES: usize = 16 * 1024;

pub(super) async fn execute(
    manager: &McpManager,
    invocation: &ToolContext,
    input: ToolSearchInput,
) -> Result<ToolResult, ToolError> {
    if input.query.len() > 512 {
        return Err(ToolError::InvalidInput(
            "tool_search query exceeds 512 bytes".into(),
        ));
    }
    let server = input
        .server
        .map(rw_types::McpServerId::new)
        .transpose()
        .map_err(mcp_tool_error)?;
    let source = manager
        .tool_search(&input.query, server.as_ref())
        .await
        .map_err(mcp_tool_error)?;
    let work = SearchWork {
        source,
        policy: invocation.mcp_tool_policy().clone(),
        raw: slot(3 * MAX_OUTPUT_BYTES + FIXED_WORK_BYTES)?,
    };
    rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
        .await
        .map_err(mcp_tool_error)?
}

struct SearchWork {
    source: McpResponse<Vec<McpToolDefinition>>,
    policy: McpToolPolicy,
    raw: McpResponseSlot,
}
impl SearchWork {
    fn run(self) -> Result<ToolResult, ToolError> {
        let encoded = encode_selected(&self.source, &self.policy)?;
        drop(self.source);
        let shape = preflight_json(
            &encoded,
            JsonStructureLimits {
                max_encoded_bytes: MAX_OUTPUT_BYTES,
                max_nodes: MAX_OUTPUT_BYTES,
                max_string_bytes: MAX_OUTPUT_BYTES,
                max_depth: 64,
            },
        )
        .map_err(mcp_tool_error)?;
        let decoded = slot(
            shape
                .direct_value_decode_bytes()
                .and_then(|bytes| bytes.checked_add(FIXED_WORK_BYTES))
                .ok_or_else(admission_error)?,
        )?;
        let data: Value = serde_json::from_slice(&encoded).map_err(mcp_tool_error)?;
        drop(encoded);
        drop(self.raw);
        let plan = rw_context::ToonAllocation::for_value(&data).ok_or_else(admission_error)?;
        let encoding = slot(
            plan.prompt_bytes
                .checked_mul(4)
                .and_then(|bytes| bytes.checked_add(plan.working_bytes))
                .and_then(|bytes| bytes.checked_add(FIXED_WORK_BYTES))
                .ok_or_else(admission_error)?,
        )?;
        let content = rw_context::encode_toon(&data).map_err(mcp_tool_error)?;
        let result = untrusted_result(&content, data);
        drop(content);
        // Result fields retire before the native carrier. Source catalog/schema
        // storage already retired after the exact output bytes were constructed.
        let retained = Arc::new((decoded.retain_native(), encoding.retain_native()));
        let payloads = ToolResultPayloads::retained(Vec::new(), retained)?;
        presentation::SEARCH.attach(result.with_payloads(payloads))
    }
}

#[derive(Serialize)]
struct SearchOutput<'a> {
    matches: Vec<&'a McpToolDefinition>,
    truncated: bool,
}
fn encode_selected(
    source: &[McpToolDefinition],
    policy: &McpToolPolicy,
) -> Result<Vec<u8>, ToolError> {
    let allowed = |definition: &&McpToolDefinition| {
        policy.allows(definition.server.as_str(), &definition.name)
    };
    let total = source.iter().filter(allowed).count();
    // At least one pointer per selected schema is covered before this allocation.
    if total
        .checked_mul(std::mem::size_of::<&McpToolDefinition>())
        .is_none_or(|bytes| bytes > FIXED_WORK_BYTES)
    {
        return Err(admission_error());
    }
    let mut selected = Vec::with_capacity(total);
    let mut array_bytes = 2usize;
    for definition in source.iter().filter(allowed) {
        let mut count = JsonWriter::count(MAX_SCHEMAS_BYTES);
        if count.serialize(definition).is_err() {
            if count.exceeded() {
                break;
            }
            return Err(admission_error());
        }
        let next = array_bytes
            .checked_add(count.written())
            .and_then(|bytes| bytes.checked_add(usize::from(!selected.is_empty())))
            .ok_or_else(admission_error)?;
        if next > MAX_SCHEMAS_BYTES {
            break;
        }
        array_bytes = next;
        selected.push(definition);
    }
    let output = SearchOutput {
        truncated: selected.len() < total,
        matches: selected,
    };
    let mut encoded = Vec::with_capacity(MAX_OUTPUT_BYTES);
    JsonWriter::buffer(&mut encoded, MAX_OUTPUT_BYTES, 0)
        .map_err(mcp_tool_error)?
        .serialize(&output)
        .map_err(mcp_tool_error)?;
    Ok(encoded)
}
fn slot(bytes: usize) -> Result<McpResponseSlot, ToolError> {
    let limits = McpResponseLimits::new(bytes).map_err(mcp_tool_error)?;
    McpResponseSlot::new(limits).map_err(mcp_tool_error)
}
fn admission_error() -> ToolError {
    ToolError::Output("MCP tool search exceeds output construction admission".into())
}

#[cfg(test)]
mod tests;
