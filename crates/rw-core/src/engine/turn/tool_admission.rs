mod approvals;
use super::tool_requests::PendingToolCall;
use rw_types::{
    allocation::PrepareAllocation,
    tool_admission::{
        MAX_PENDING_TOOL_ARGUMENT_BYTES, MAX_PENDING_TOOL_INVOCATIONS,
        MAX_PENDING_TOOL_PREPARED_BYTES, MAX_TOOL_CALL_ID_BYTES, MAX_TOOL_NAME_BYTES,
    },
};
use serde_json::Value;

/// Charges the unique batch payload before its bounded preparation copies are made.
/// Provider/command decoders independently own their incoming frame allocations.
#[derive(Default)]
pub(super) struct PendingToolBudget {
    calls: usize,
    encoded: usize,
    prepared: usize,
    streamed: usize,
    approval_encoded: usize,
    approval_prepared: usize,
}
impl PendingToolBudget {
    pub(super) fn from_calls(calls: &[PendingToolCall]) -> Result<Self, String> {
        let mut budget = Self::default();
        for call in calls {
            budget.start(&call.id, &call.name)?;
            if let Some(arguments) = &call.arguments {
                budget.arguments(arguments)?;
            }
        }
        Ok(budget)
    }
    pub(super) fn start(&mut self, id: &String, name: &String) -> Result<(), String> {
        if self.calls == MAX_PENDING_TOOL_INVOCATIONS
            || id.is_empty()
            || id.len() > MAX_TOOL_CALL_ID_BYTES
            || name.is_empty()
            || name.len() > MAX_TOOL_NAME_BYTES
        {
            return Err("pending tool invocation count or identity exceeds admission".into());
        }
        let metadata = id
            .capacity()
            .checked_add(name.capacity())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<PendingToolCall>()))
            .ok_or("pending tool metadata allocation overflow")?;
        let prepared = self
            .prepared
            .checked_add(metadata)
            .filter(|bytes| *bytes <= MAX_PENDING_TOOL_PREPARED_BYTES)
            .ok_or("pending tool prepared allocation exceeds admission")?;
        self.prepared = prepared;
        self.calls += 1;
        Ok(())
    }
    pub(super) fn delta(&mut self, fragment: &str) -> Result<(), String> {
        self.streamed = self
            .streamed
            .checked_add(fragment.len())
            .filter(|bytes| *bytes <= MAX_PENDING_TOOL_ARGUMENT_BYTES)
            .ok_or("pending tool argument stream exceeds admission")?;
        Ok(())
    }
    pub(super) fn arguments(&mut self, arguments: &Value) -> Result<(), String> {
        self.replace(None, arguments)
    }
    pub(super) fn replace(&mut self, old: Option<&Value>, next: &Value) -> Result<(), String> {
        let (old_encoded, old_prepared) = old.map(measure).transpose()?.unwrap_or_default();
        let (encoded, prepared) = measure(next)?;
        let encoded = self
            .encoded
            .checked_sub(old_encoded)
            .and_then(|n| n.checked_add(encoded))
            .filter(|n| *n <= MAX_PENDING_TOOL_ARGUMENT_BYTES)
            .ok_or("pending tool argument bytes exceed admission")?;
        let prepared = self
            .prepared
            .checked_sub(old_prepared)
            .and_then(|n| n.checked_add(prepared))
            .filter(|n| *n <= MAX_PENDING_TOOL_PREPARED_BYTES)
            .ok_or("pending tool prepared allocation exceeds admission")?;
        self.encoded = encoded;
        self.prepared = prepared;
        Ok(())
    }
}
fn measure(value: &Value) -> Result<(usize, usize), String> {
    let prepared = value
        .prepared_bytes()
        .filter(|bytes| *bytes <= MAX_PENDING_TOOL_PREPARED_BYTES)
        .ok_or("pending tool argument allocation exceeds admission")?;
    let encoded = encoded_bytes(value, MAX_PENDING_TOOL_ARGUMENT_BYTES)
        .map_err(|_| "pending tool argument bytes exceed admission".to_owned())?;
    Ok((encoded, prepared))
}
fn encoded_bytes(value: &impl serde::Serialize, limit: usize) -> Result<usize, String> {
    let mut counter = rw_types::json_encoding::JsonWriter::count(limit);
    counter
        .serialize(value)
        .map_err(|_| "tool payload byte admission".to_owned())?;
    Ok(counter.written())
}

#[cfg(test)]
mod tests;

/// Owns the admitted execution payloads and the exact separately admitted announcements.
pub(super) struct AdmittedToolBatch {
    pub(super) calls: Vec<(PendingToolCall, Value)>,
    pub(super) budget: PendingToolBudget,
}
impl AdmittedToolBatch {
    pub(super) fn new(
        calls: Vec<PendingToolCall>,
        redactor: &dyn crate::engine::SecretRedactor,
    ) -> Result<Self, String> {
        let budget = PendingToolBudget::from_calls(&calls)?;
        if calls.iter().any(|call| call.arguments.is_none()) {
            return Err("tool batch admission requires complete arguments".into());
        }
        let mut display_budget = PendingToolBudget::default();
        let mut admitted = Vec::with_capacity(calls.len());
        for call in calls {
            display_budget.start(&call.id, &call.name)?;
            let displayed = super::redaction::redacted_json(
                call.arguments.clone().unwrap_or(Value::Null),
                redactor,
            );
            display_budget.arguments(&displayed)?;
            admitted.push((call, displayed));
        }
        Ok(Self {
            calls: admitted,
            budget,
        })
    }
}
