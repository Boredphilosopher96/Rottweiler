//! Profile of the concrete rmcp 3.1.2 graphs selected by `messages`.
//!
//! JSON-RPC and `ServerResult` trial unions are never decoded. Direct result
//! graphs need at most four simultaneous representations: final typed storage,
//! `ContentBlock`'s tagged buffer, `ResourceContents`' trial buffer and its current
//! attempt. MRTR/request graphs additionally contain `InputRequest`, request params
//! flattening, elicitation mode and the primitive/enum/select schema trials.
//! Attempts are sequential; twelve representations cover their deepest chain,
//! including the final typed graph. These peaks also cover result conversion:
//! the retained typed graph and a newly constructed generic `Value` coexist under
//! the same credit (two representations, including each one's capacity growth).
//! A request serializer's `WithMeta` temporary and metadata merge fit within its
//! twelve-representation profile. The owner must retain this credit through
//! conversion; it must not refund it when a request-count slot is released.
//! Arbitrary JSON leaves decode directly as
//! `Value` and add no recursive typed trials. Admission includes capacity growth.
use super::invalid;
use rmcp::model::{
    ContentBlock, DiscoverResult, ElicitRequestParams, GetTaskResult, Implementation,
    InitializeResult, InputRequest, PrimitiveSchemaDefinition, Prompt, PromptMessage, Resource,
    ResourceContents, ResourceTemplate, ServerCapabilities, SubscriptionFilter, Tool,
};
use rw_types::json_structure::{JsonStructure, JsonStructureLimits, preflight_json};
use std::{io, mem::size_of};

pub(super) fn inspect(input: &[u8]) -> io::Result<JsonStructure> {
    preflight_json(
        input,
        JsonStructureLimits {
            max_encoded_bytes: 64 * 1024 * 1024,
            max_nodes: 65_536,
            max_string_bytes: 64 * 1024 * 1024,
            max_depth: 64,
        },
    )
    .map_err(super::json_error)
}

pub(super) fn working_bytes(shape: &JsonStructure, request_graph: bool) -> io::Result<usize> {
    let slot = slot_bytes(request_graph);
    // A BTreeMap's first node can contain eleven key/value slots. Sixteen
    // slots per object cover that first node and headers; four slots per entry
    // cover subsequent nodes or IndexMap/hash-table simultaneous growth.
    // Vec capacity and reallocation overlap are charged at four element slots.
    let bytes = (|| {
        let values = shape
            .nodes
            .checked_add(shape.array_entries.checked_mul(4)?)?
            .checked_mul(slot)?;
        let maps = shape
            .objects
            .checked_mul(16)?
            .checked_add(shape.object_entries.checked_mul(4)?)?
            .checked_mul(slot.checked_add(size_of::<String>())?)?;
        let strings = shape.string_bytes.checked_mul(2)?;
        values
            .checked_add(maps)?
            .checked_add(strings)?
            .checked_mul(if request_graph { 12 } else { 4 })?
            .checked_add(16 * 1024)
    })()
    .ok_or_else(|| invalid("MCP decoded allocation profile overflow"))?;
    if bytes > 128 * 1024 * 1024 {
        return Err(invalid("MCP decoded allocation admission exceeded"));
    }
    Ok(bytes)
}

// Every dynamic collection element in the admitted result families is a Value,
// string, one of these records, or a smaller inline member of one of them.
// Request-only boxed records are included explicitly rather than measuring Box.
#[allow(deprecated)] // Sampling remains an external MCP request shape.
fn slot_bytes(request_graph: bool) -> usize {
    let direct = [
        size_of::<serde_json::Value>(),
        size_of::<ContentBlock>(),
        size_of::<ResourceContents>(),
        size_of::<Resource>(),
        size_of::<ResourceTemplate>(),
        size_of::<Tool>(),
        size_of::<Prompt>(),
        size_of::<PromptMessage>(),
        size_of::<ServerCapabilities>(),
        size_of::<InitializeResult>(),
        size_of::<DiscoverResult>(),
        size_of::<Implementation>(),
        size_of::<SubscriptionFilter>(),
        size_of::<GetTaskResult>(),
    ]
    .into_iter()
    .max()
    .unwrap_or(128)
    .max(128);
    if request_graph {
        direct
            .max(size_of::<InputRequest>())
            .max(size_of::<ElicitRequestParams>())
            .max(size_of::<PrimitiveSchemaDefinition>())
            .max(size_of::<rmcp::model::CreateMessageRequestParams>())
            .max(size_of::<rmcp::model::SamplingMessageContentBlock>())
    } else {
        direct
    }
}
