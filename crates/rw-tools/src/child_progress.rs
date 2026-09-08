//! Construction and delivery share one publisher-owned child preview allowance.
use crate::ToolError;
use rw_types::{
    json_encoding::JsonWriter,
    json_structure::{JsonStructureLimits, preflight_json},
};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MAX_CHILD_PROGRESS_BYTES: usize = 256 * 1024;
pub const CHILD_PROGRESS_MEMORY_BYTES: usize = 8 * 1024 * 1024;

/// Shared by every producer and queue slot belonging to one parent publisher.
#[derive(Clone, Debug)]
pub struct ChildProgressBudget(Arc<Semaphore>);
impl Default for ChildProgressBudget {
    fn default() -> Self {
        Self(Arc::new(Semaphore::new(CHILD_PROGRESS_MEMORY_BYTES)))
    }
}
impl ChildProgressBudget {
    /// Build a preview only after reserving encoded capacity and parser scratch.
    /// Decode admission is added before constructing the JSON value. Saturation
    /// yields a canonical invalidation, or drops an unsequenced display update.
    ///
    /// # Errors
    /// Preserves serialization errors and rejects oversized/unrepresentable
    /// previews without a canonical sequence from which to recover them.
    pub fn encode(
        &self,
        sequence: Option<u64>,
        source: &rw_types::EngineEvent,
    ) -> Result<Option<ChildProgressPreview>, ToolError> {
        self.construct(sequence, source)
    }

    /// Admit a native observer's JSON preview under the same construction contract.
    ///
    /// # Errors
    /// Rejects an oversized or unsupported preview without a canonical sequence.
    pub fn encode_value(
        &self,
        sequence: Option<u64>,
        source: &Value,
    ) -> Result<Option<ChildProgressPreview>, ToolError> {
        self.construct(sequence, source)
    }

    fn construct<T: Serialize + ?Sized>(
        &self,
        sequence: Option<u64>,
        source: &T,
    ) -> Result<Option<ChildProgressPreview>, ToolError> {
        let Some(scratch) = self.reserve(3 * MAX_CHILD_PROGRESS_BYTES) else {
            return Ok(sequence.map(|_| ChildProgressPreview::invalidation()));
        };
        let mut bytes = Vec::new();
        {
            let mut writer =
                JsonWriter::buffer(&mut bytes, MAX_CHILD_PROGRESS_BYTES, 1024).map_err(output)?;
            if let Err(error) = writer.serialize(source) {
                return if writer.exceeded() {
                    invalidation(sequence).map(Some)
                } else {
                    Err(output(error))
                };
            }
        }
        let shape = match preflight_json(
            &bytes,
            JsonStructureLimits {
                max_encoded_bytes: MAX_CHILD_PROGRESS_BYTES,
                max_nodes: 65_536,
                max_string_bytes: MAX_CHILD_PROGRESS_BYTES,
                max_depth: 62,
            },
        ) {
            Ok(shape) => shape,
            Err(_) => return invalidation(sequence).map(Some),
        };
        let decoded = shape
            .direct_value_decode_bytes()
            .ok_or_else(|| output("child preview decode overflow"))?;
        let Some(retained) = self.reserve(decoded) else {
            return Ok(sequence.map(|_| ChildProgressPreview::invalidation()));
        };
        // Keep the conservative direct-decode peak through delivery. A freshly
        // decoded map needs no second normalization or allocation traversal.
        let preview = ChildProgressPreview {
            value: serde_json::from_slice(&bytes).map_err(output)?,
            retained: Some(retained),
        };
        drop(bytes);
        drop(scratch);
        if preview.value.is_null() {
            invalidation(sequence).map(Some)
        } else {
            Ok(Some(preview))
        }
    }

    /// Reserve queue/identity metadata from the same physical allowance.
    #[must_use]
    pub fn reserve(&self, bytes: usize) -> Option<ChildProgressRetention> {
        let count = u32::try_from(bytes).ok()?;
        Some(ChildProgressRetention {
            _permit: self.0.clone().try_acquire_many_owned(count).ok()?,
            budget: self.clone(),
        })
    }

    /// Remaining physical construction and queued-preview allowance.
    #[must_use]
    pub fn available_bytes(&self) -> usize {
        self.0.available_permits()
    }

    /// Reject accidental transfer from a different parent publisher's budget.
    #[must_use]
    pub fn owns(&self, preview: &ChildProgressPreview) -> bool {
        preview
            .retained
            .as_ref()
            .is_none_or(|retained| Arc::ptr_eq(&self.0, &retained.budget.0))
    }
}

/// The value retires before its credit, including abandoned callback futures.
#[derive(Debug)]
pub struct ChildProgressPreview {
    value: Value,
    retained: Option<ChildProgressRetention>,
}
impl ChildProgressPreview {
    #[must_use]
    pub const fn invalidation() -> Self {
        Self {
            value: Value::Null,
            retained: None,
        }
    }
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.value
    }
    pub fn invalidate(&mut self) {
        self.value = Value::Null;
        self.retained = None;
    }
    /// Deliver into a synchronous consumer that establishes its own admission.
    /// Credit remains owned through the complete callback, including unwinding.
    pub fn deliver(mut self, publish: impl FnOnce(Value)) {
        publish(std::mem::replace(&mut self.value, Value::Null));
    }
}

/// Physical credit for a constructed preview or its queue metadata.
#[derive(Debug)]
pub struct ChildProgressRetention {
    _permit: OwnedSemaphorePermit,
    budget: ChildProgressBudget,
}
fn invalidation(sequence: Option<u64>) -> Result<ChildProgressPreview, ToolError> {
    sequence
        .map(|_| ChildProgressPreview::invalidation())
        .ok_or_else(|| output("child progress invalidation requires a canonical sequence"))
}
fn output(error: impl std::fmt::Display) -> ToolError {
    ToolError::Output(error.to_string())
}

#[cfg(test)]
mod tests;
