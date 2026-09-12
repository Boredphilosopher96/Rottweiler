//! Structural inspection precedes every role-specific typed graph allocation.
use rw_types::json_structure::{JsonStructure, JsonStructureLimits, preflight_json};
use std::{io, mem::size_of};

pub(crate) fn inspect(input: &[u8]) -> io::Result<JsonStructure> {
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

pub(crate) fn working_bytes(
    shape: &JsonStructure,
    slot: usize,
    representations: usize,
) -> io::Result<usize> {
    // Account for initial BTreeMap nodes, later nodes/hash-table growth, and
    // old/new Vec capacity. The role decoder supplies its concrete type width
    // and maximum simultaneous typed/trial/consumer graph representations.
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
            .checked_mul(representations)?
            .checked_add(16 * 1024)
    })()
    .ok_or_else(|| super::invalid("MCP decoded allocation profile overflow"))?;
    if bytes > 128 * 1024 * 1024 {
        return Err(super::invalid("MCP decoded allocation admission exceeded"));
    }
    Ok(bytes)
}
