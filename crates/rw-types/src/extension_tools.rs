//! Invocation-bound access to the session's approved tool execution path.
use crate::{ToolInvocationId, ToolOutput, TurnId, extension_invocation::ExtensionInvocationId};
use rw_memory_derive::PrepareAllocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_EXTENSION_TOOL_INPUT_BYTES: usize = 256 * 1024;
pub const MAX_EXTENSION_TOOL_INPUT_PREPARED_BYTES: usize = 1024 * 1024;
pub const MAX_EXTENSION_TOOL_OUTPUT_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, PrepareAllocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolCall {
    pub origin: ExtensionInvocationId,
    #[schemars(length(min = 1, max = 256))]
    pub name: String,
    #[schemars(extend("x-rw-max-json-bytes" = MAX_EXTENSION_TOOL_INPUT_BYTES))]
    pub input: serde_json::Value,
}
impl ExtensionToolCall {
    /// # Errors
    /// Rejects oversized identities and input before actor admission.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.is_empty() || self.name.len() > crate::tool_admission::MAX_TOOL_NAME_BYTES {
            return Err("invalid host tool identity");
        }
        use crate::allocation::PrepareAllocation as _;
        if self
            .input
            .prepared_bytes()
            .is_none_or(|bytes| bytes > MAX_EXTENSION_TOOL_INPUT_PREPARED_BYTES)
        {
            return Err("host tool input allocation exceeds admission");
        }
        if !within_json_limit(&self.input, MAX_EXTENSION_TOOL_INPUT_BYTES) {
            return Err("host tool input exceeds its byte limit");
        }
        Ok(())
    }
}

/// Full output stays in the canonical tool event. An oversized callback output
/// is absent, never silently substituted with a partial structured value.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, PrepareAllocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolOutcome {
    pub turn_id: TurnId,
    pub invocation_id: ToolInvocationId,
    pub is_error: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with="crate::schema::required_nullable::<ToolOutput>", extend("x-rw-max-json-bytes" = MAX_EXTENSION_TOOL_OUTPUT_BYTES))]
    pub output: Option<ToolOutput>,
}

/// Counts encoded bytes without allocating an intermediate JSON document.
#[must_use]
pub fn within_json_limit(value: &impl Serialize, limit: usize) -> bool {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("tool byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(limit), value).is_ok()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{ExtensionToolCall, ExtensionToolOutcome, MAX_EXTENSION_TOOL_INPUT_BYTES};
    use serde_json::json;
    #[test]
    fn host_tool_requests_bind_origin_and_bound_retained_inputs() {
        let request = json!({"origin":"ab".repeat(16),"name":"read","input":{"path":"file"}});
        serde_json::from_value::<ExtensionToolCall>(request.clone())
            .expect("request")
            .validate()
            .expect("bounded");
        for field in ["origin", "name", "input"] {
            let mut incomplete = request.clone();
            incomplete.as_object_mut().expect("object").remove(field);
            assert!(serde_json::from_value::<ExtensionToolCall>(incomplete).is_err());
        }
        let mut forged = request.clone();
        forged["session_id"] = json!("other");
        assert!(serde_json::from_value::<ExtensionToolCall>(forged).is_err());
        let mut oversized = request;
        oversized["input"] = json!("x".repeat(MAX_EXTENSION_TOOL_INPUT_BYTES));
        assert!(
            serde_json::from_value::<ExtensionToolCall>(oversized)
                .expect("typed")
                .validate()
                .is_err()
        );
    }
    #[test]
    fn absent_callback_output_is_explicit_and_keeps_canonical_identity() {
        let result = json!({"turn_id":"1","invocation_id":"turn-1:extension-0","is_error":false,"output":null});
        serde_json::from_value::<ExtensionToolOutcome>(result.clone())
            .expect("explicit source-only output");
        let mut incomplete = result;
        incomplete.as_object_mut().expect("object").remove("output");
        assert!(serde_json::from_value::<ExtensionToolOutcome>(incomplete).is_err());
    }
}
