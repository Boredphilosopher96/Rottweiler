//! Producer records use the same response carrier as MCP clients.
use super::{EngineTool, SessionSummary};
use rw_types::allocation::{DecodeAllocation, PrepareAllocation};
use std::mem::size_of;

impl DecodeAllocation for SessionSummary {
    fn decode_node_bytes() -> Option<usize> {
        Some(size_of::<Self>())
    }
}
impl PrepareAllocation for SessionSummary {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        self.id.capacity().checked_add(self.state.capacity())
    }
    fn prepare_allocations(&mut self) {}
}
impl DecodeAllocation for EngineTool {
    fn decode_node_bytes() -> Option<usize> {
        Some(size_of::<Self>().max(serde_json::Value::decode_node_bytes()?))
    }
}
impl PrepareAllocation for EngineTool {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        self.name
            .capacity()
            .checked_add(self.description.capacity())?
            .checked_add(self.input_schema.prepared_heap_bytes()?)
    }
    fn prepare_allocations(&mut self) {
        self.input_schema.prepare_allocations();
    }
}
