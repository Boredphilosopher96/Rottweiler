use super::{PendingToolBudget, encoded_bytes};
use crate::PermissionRequest;
use rw_types::{
    allocation::PrepareAllocation,
    tool_admission::{MAX_PENDING_TOOL_APPROVAL_BYTES, MAX_PENDING_TOOL_APPROVAL_PREPARED_BYTES},
};

impl PendingToolBudget {
    /// A whole-batch charge remains held through every approval and execution.
    /// Reauthorization charges another revision before publishing its preview.
    pub(in crate::engine::turn) fn approval_payload(
        &mut self,
        request: &PermissionRequest,
        copies: usize,
    ) -> Result<(), String> {
        let encoded = encoded_bytes(request, MAX_PENDING_TOOL_APPROVAL_BYTES)?
            .checked_add(1024)
            .and_then(|bytes| bytes.checked_mul(copies))
            .and_then(|bytes| bytes.checked_add(self.approval_encoded))
            .filter(|bytes| *bytes <= MAX_PENDING_TOOL_APPROVAL_BYTES)
            .ok_or("pending tool approval bytes exceed admission")?;
        let prepared = request_bytes(request)
            // Request, security binding, actor pending state, and event publication
            // have independent owned copies; none borrows the producer frame.
            .and_then(|bytes| bytes.checked_add(1024))
            .and_then(|bytes| bytes.checked_mul(4))
            .and_then(|bytes| bytes.checked_mul(copies))
            .and_then(|bytes| bytes.checked_add(self.approval_prepared))
            .filter(|bytes| *bytes <= MAX_PENDING_TOOL_APPROVAL_PREPARED_BYTES)
            .ok_or("pending tool approval allocation exceeds admission")?;
        self.approval_encoded = encoded;
        self.approval_prepared = prepared;
        Ok(())
    }
}

fn request_bytes(request: &PermissionRequest) -> Option<usize> {
    [
        Some(std::mem::size_of::<PermissionRequest>()),
        Some(request.id.capacity()),
        Some(request.invocation_id.0.capacity()),
        Some(request.tool_name.capacity()),
        request.arguments.prepared_bytes(),
        request.capabilities.prepared_bytes(),
        request.approval_diff.prepared_bytes(),
    ]
    .into_iter()
    .try_fold(0usize, |total, bytes| total.checked_add(bytes?))
}
