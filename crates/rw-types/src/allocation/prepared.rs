use super::PrepareAllocation;
use serde::{Serialize, Serializer};

/// Owns a value while its prepared allocation bound awaits admission.
///
/// Creation only traverses the existing value; it allocates no normalization
/// scratch. The original value still belongs to its producer's memory allowance.
#[derive(Debug)]
pub struct AllocationPlan<T> {
    value: T,
    bytes: usize,
}

impl<T: PrepareAllocation> AllocationPlan<T> {
    /// Computes a checked charge without modifying or copying the value.
    ///
    /// # Errors
    /// Returns the original value on overflow or unsupported JSON nesting.
    pub fn new(value: T) -> Result<Self, T> {
        match value.prepared_bytes() {
            Some(bytes) => Ok(Self { value, bytes }),
            None => Err(value),
        }
    }

    /// New allocation allowance required before preparation starts, and retained
    /// value allowance required afterward. Allocator bookkeeping is excluded.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    #[must_use]
    pub const fn value(&self) -> &T {
        &self.value
    }

    /// Normalizes opaque allocations after the caller reserves `bytes()`.
    /// No mutable reference to the prepared value escapes this boundary.
    #[must_use]
    pub fn prepare(mut self) -> PreparedAllocation<T> {
        self.value.prepare_allocations();
        PreparedAllocation {
            value: self.value,
            bytes: self.bytes,
        }
    }
}

/// Immutable value whose prepared allocation bound has been established.
/// Deliberately has no `Clone`, mutable dereference, or unchecked constructor.
#[derive(Debug)]
pub struct PreparedAllocation<T> {
    value: T,
    bytes: usize,
}

impl<T> PreparedAllocation<T> {
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    #[must_use]
    pub const fn value(&self) -> &T {
        &self.value
    }
}

impl<T> PreparedAllocation<T> {
    /// Transfers the value to its next allocation owner.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T: Serialize> Serialize for PreparedAllocation<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}
