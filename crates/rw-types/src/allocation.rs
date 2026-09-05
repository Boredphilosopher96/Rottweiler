//! Prepared allocation bounds for typed admission, excluding allocator bookkeeping.
//!
//! Strings and vectors charge their actual capacity. JSON object backing storage is
//! private, so preparation moves its entries into a fresh map without changing their
//! order. The bound covers that new map, not an arbitrary original map reservation.
//! Original producer memory remains that producer's responsibility until released.
//! Admission must reserve the reported bytes before calling `AllocationPlan::prepare`.

mod prepared;
pub use prepared::{AllocationPlan, PreparedAllocation};

use std::{
    collections::BTreeMap,
    mem::{align_of, size_of},
};

/// Checked upper bound after preparing an owned value's nested allocations.
/// `None` means overflow or unsupported nesting and requires rejection.
/// Implementations must preserve values and serialization order during preparation.
pub trait PrepareAllocation {
    fn prepared_heap_bytes(&self) -> Option<usize>;

    fn prepare_allocations(&mut self);

    fn prepared_bytes(&self) -> Option<usize> {
        size_of_val(self).checked_add(self.prepared_heap_bytes()?)
    }
}

macro_rules! inline {
    ($($ty:ty),* $(,)?) => {$(impl PrepareAllocation for $ty {
        fn prepared_heap_bytes(&self) -> Option<usize> { Some(0) }
        fn prepare_allocations(&mut self) {}
    })*};
}
inline!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64
);

impl PrepareAllocation for String {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        Some(self.capacity())
    }
    fn prepare_allocations(&mut self) {}
}
impl<T: PrepareAllocation> PrepareAllocation for Vec<T> {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        self.iter().try_fold(
            self.capacity().checked_mul(size_of::<T>())?,
            |total, value| total.checked_add(value.prepared_heap_bytes()?),
        )
    }
    fn prepare_allocations(&mut self) {
        for value in self {
            value.prepare_allocations();
        }
    }
}
impl<T: PrepareAllocation> PrepareAllocation for Option<T> {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        self.as_ref()
            .map_or(Some(0), PrepareAllocation::prepared_heap_bytes)
    }
    fn prepare_allocations(&mut self) {
        if let Some(value) = self {
            value.prepare_allocations();
        }
    }
}
impl<T: PrepareAllocation, const N: usize> PrepareAllocation for [T; N] {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        self.iter().try_fold(0usize, |total, value| {
            total.checked_add(value.prepared_heap_bytes()?)
        })
    }
    fn prepare_allocations(&mut self) {
        for value in self {
            value.prepare_allocations();
        }
    }
}
impl<T: PrepareAllocation> PrepareAllocation for Box<T> {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        self.as_ref().prepared_bytes()
    }
    fn prepare_allocations(&mut self) {
        self.as_mut().prepare_allocations();
    }
}
impl<V: PrepareAllocation> PrepareAllocation for BTreeMap<String, V> {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        // Pinned Rust's B-tree has at most 11 key/value slots and 12 child pointers
        // per node. Charge a whole overprovisioned node per entry plus one empty
        // root; removing the last entry may retain that root allocation.
        self.iter().try_fold(
            map_nodes::<String, V>(self.len())?,
            |total, (key, value)| {
                total
                    .checked_add(key.prepared_heap_bytes()?)?
                    .checked_add(value.prepared_heap_bytes()?)
            },
        )
    }
    fn prepare_allocations(&mut self) {
        for value in self.values_mut() {
            value.prepare_allocations();
        }
    }
}
fn map_nodes<K, V>(entries: usize) -> Option<usize> {
    // One conservative B-tree node per entry (11 slots, 12 edges on Rust 1.97.1).
    // This also exceeds fresh IndexMap 2.14 storage: one bucket per entry plus
    // its hash table, rounded capacity, control bytes, and alignment padding.
    let node = size_of::<(K, V)>()
        .checked_mul(16)?
        .checked_add(256)?
        .checked_add(align_of::<(K, V)>().checked_mul(2)?)?;
    entries.checked_add(1)?.checked_mul(node)
}

impl PrepareAllocation for serde_json::Value {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        json_heap_bytes(self, 0)
    }
    fn prepare_allocations(&mut self) {
        match self {
            Self::Array(values) => {
                for value in values {
                    value.prepare_allocations();
                }
            }
            Self::Object(values) => {
                for value in values.values_mut() {
                    value.prepare_allocations();
                }
                *values = std::mem::take(values).into_iter().collect();
            }
            _ => {}
        }
    }
}
const MAX_JSON_DEPTH: usize = 128;

fn json_heap_bytes(value: &serde_json::Value, depth: usize) -> Option<usize> {
    use serde_json::Value;
    if depth > MAX_JSON_DEPTH {
        return None;
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(0),
        Value::String(text) => text.prepared_heap_bytes(),
        Value::Array(values) => values.iter().try_fold(
            values.capacity().checked_mul(size_of::<Value>())?,
            |total, value| total.checked_add(json_heap_bytes(value, depth + 1)?),
        ),
        Value::Object(values) => values.iter().try_fold(
            map_nodes::<String, Value>(values.len())?,
            |total, (key, value)| {
                total
                    .checked_add(key.capacity())?
                    .checked_add(json_heap_bytes(value, depth + 1)?)
            },
        ),
    }
}

// The admission model requires serde_json's inline numeric representation.
// Enabling arbitrary_precision needs its owned number storage accounted first.
const _: () = assert!(size_of::<serde_json::Number>() <= size_of::<u128>());
#[cfg(test)]
mod tests;

impl PrepareAllocation for rw_operation_contract::ToolProgress {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        Some(self.retained_message_capacity())
    }
    fn prepare_allocations(&mut self) {}
}
impl PrepareAllocation for rw_operation_contract::ProgressAmount {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        Some(0)
    }
    fn prepare_allocations(&mut self) {}
}
