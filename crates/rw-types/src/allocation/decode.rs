//! Decode profile for the closed typed wire graph and serde's owned intermediates.
use super::map_nodes;
use std::{collections::BTreeMap, mem::size_of};

/// Maximum backing allocation per JSON node, before nesting payloads are added.
///
/// Implementations cover normal serde deserialization: vector capacity growth,
/// map nodes, boxes and the largest nested element. Custom deserializers must not
/// allocate unrepresented payloads. Source-derived profiles traverse every field.
/// String payloads and serde intermediates are charged separately by JsonStructure.
pub trait DecodeAllocation {
    fn decode_node_bytes() -> Option<usize>;
}
impl DecodeAllocation for String {
    fn decode_node_bytes() -> Option<usize> {
        Some(size_of::<Self>())
    }
}
impl<T: DecodeAllocation> DecodeAllocation for Vec<T> {
    fn decode_node_bytes() -> Option<usize> {
        Some(T::decode_node_bytes()?.max(size_of::<T>().checked_mul(2)?.checked_add(64)?))
    }
}
impl<T: DecodeAllocation> DecodeAllocation for Option<T> {
    fn decode_node_bytes() -> Option<usize> {
        Some(T::decode_node_bytes()?.max(size_of::<Self>()))
    }
}
impl<T: DecodeAllocation, const N: usize> DecodeAllocation for [T; N] {
    fn decode_node_bytes() -> Option<usize> {
        Some(T::decode_node_bytes()?.max(size_of::<Self>()))
    }
}
impl<T: DecodeAllocation> DecodeAllocation for Box<T> {
    fn decode_node_bytes() -> Option<usize> {
        Some(T::decode_node_bytes()?.max(size_of::<T>()))
    }
}
impl<T: DecodeAllocation> DecodeAllocation for BTreeMap<String, T> {
    fn decode_node_bytes() -> Option<usize> {
        Some(T::decode_node_bytes()?.max(map_nodes::<String, T>(1)?))
    }
}
impl DecodeAllocation for serde_json::Value {
    fn decode_node_bytes() -> Option<usize> {
        map_nodes::<String, Self>(1)
    }
}
impl DecodeAllocation for rw_operation_contract::ToolProgress {
    fn decode_node_bytes() -> Option<usize> {
        Some(size_of::<Self>().max(size_of::<String>()))
    }
}
impl DecodeAllocation for rw_operation_contract::ProgressAmount {
    fn decode_node_bytes() -> Option<usize> {
        Some(size_of::<Self>())
    }
}

impl<T: DecodeAllocation + ?Sized> DecodeAllocation for &T {
    fn decode_node_bytes() -> Option<usize> {
        T::decode_node_bytes()
    }
}

impl DecodeAllocation for str {
    fn decode_node_bytes() -> Option<usize> {
        Some(size_of::<String>())
    }
}
