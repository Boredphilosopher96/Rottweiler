//! Shared, transport-independent limits and validated values for local operations.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;
use ts_rs::TS;

pub const MAX_OPERATION_DURATION_MS: u32 = 300_000;
pub const DEFAULT_TOOL_IDLE_TIMEOUT_MS: u32 = 90_000;
pub const MAX_PROGRESS_MESSAGE_CHARS: usize = 256;
pub const MAX_PROGRESS_MESSAGE_BYTES: usize = 4 * MAX_PROGRESS_MESSAGE_CHARS;
pub const MAX_PROGRESS_FRAME_BYTES: usize = 4096;
pub const PROGRESS_INTERVAL_MS: u32 = 250;
pub const PROGRESS_BURST: u32 = 4;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("invalid operation contract: {0}")]
pub struct ContractError(&'static str);

/// Total time never renews. Valid progress may renew idle time within that total.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OperationLifetime {
    #[schemars(range(min = 1, max = 300_000))]
    total_ms: u32,
    #[schemars(range(min = 1, max = 300_000))]
    idle_ms: u32,
}

impl OperationLifetime {
    /// # Errors
    /// Rejects zero, excessive, or contradictory deadlines.
    pub fn new(total_ms: u32, idle_ms: u32) -> Result<Self, ContractError> {
        if total_ms == 0
            || total_ms > MAX_OPERATION_DURATION_MS
            || idle_ms == 0
            || idle_ms > total_ms
        {
            return Err(ContractError("expected 0 < idle_ms <= total_ms <= 300000"));
        }
        Ok(Self { total_ms, idle_ms })
    }
    #[must_use]
    pub const fn total_ms(self) -> u32 {
        self.total_ms
    }
    #[must_use]
    pub const fn idle_ms(self) -> u32 {
        self.idle_ms
    }
}

impl Default for OperationLifetime {
    fn default() -> Self {
        Self {
            total_ms: MAX_OPERATION_DURATION_MS,
            idle_ms: DEFAULT_TOOL_IDLE_TIMEOUT_MS,
        }
    }
}

impl<'de> Deserialize<'de> for OperationLifetime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            total_ms: u32,
            idle_ms: u32,
        }
        let value = Wire::deserialize(deserializer)?;
        Self::new(value.total_ms, value.idle_ms).map_err(D::Error::custom)
    }
}

/// Optional bounded work-count observation. It cannot grant more execution time.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProgressAmount {
    completed: u32,
    #[schemars(range(min = 1))]
    total: u32,
}

impl ProgressAmount {
    /// # Errors
    /// Rejects an empty total or a completed count beyond that total.
    pub fn new(completed: u32, total: u32) -> Result<Self, ContractError> {
        if total == 0 || completed > total {
            return Err(ContractError("expected completed <= total and total > 0"));
        }
        Ok(Self { completed, total })
    }
    #[must_use]
    pub const fn completed(self) -> u32 {
        self.completed
    }
    #[must_use]
    pub const fn total(self) -> u32 {
        self.total
    }
}

impl<'de> Deserialize<'de> for ProgressAmount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            completed: u32,
            total: u32,
        }
        let value = Wire::deserialize(deserializer)?;
        Self::new(value.completed, value.total).map_err(D::Error::custom)
    }
}

/// Replaceable display state; never authoritative tool output or a durable record.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolProgress {
    #[schemars(
        length(min = 1, max = 256),
        regex(pattern = r"^[^\u0000-\u001f\u007f-\u009f]+(?![\s\S])")
    )]
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    amount: Option<ProgressAmount>,
}

impl ToolProgress {
    /// # Errors
    /// Rejects empty/oversized text and terminal control characters.
    pub fn new(message: String, amount: Option<ProgressAmount>) -> Result<Self, ContractError> {
        if message.is_empty()
            || message.len() > MAX_PROGRESS_MESSAGE_BYTES
            || message.chars().count() > MAX_PROGRESS_MESSAGE_CHARS
            || message.chars().any(char::is_control)
        {
            return Err(ContractError("progress message must be bounded plain text"));
        }
        Ok(Self { message, amount })
    }
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Requested owned string allocation, including unused capacity.
    #[must_use]
    pub const fn retained_message_capacity(&self) -> usize {
        self.message.capacity()
    }
    #[must_use]
    pub const fn amount(&self) -> Option<ProgressAmount> {
        self.amount
    }
}

impl<'de> Deserialize<'de> for ToolProgress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            message: String,
            amount: Option<ProgressAmount>,
        }
        let value = Wire::deserialize(deserializer)?;
        Self::new(value.message, value.amount).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_parsing_enforces_semantic_and_utf8_bounds() {
        for value in [
            json!({"total_ms":0,"idle_ms":1}),
            json!({"total_ms":10,"idle_ms":11}),
            json!({"total_ms":300_001,"idle_ms":1}),
        ] {
            assert!(serde_json::from_value::<OperationLifetime>(value).is_err());
        }
        for value in [
            json!({"message":"","amount":null}),
            json!({"message":"bad\u{1b}","amount":null}),
            json!({"message":"é".repeat(513),"amount":null}),
            json!({"message":"work","amount":{"completed":2,"total":1}}),
        ] {
            assert!(serde_json::from_value::<ToolProgress>(value).is_err());
        }
        assert!(
            serde_json::from_value::<ToolProgress>(
                json!({"message":"work","amount":{"completed":1,"total":2}})
            )
            .is_ok()
        );
    }
}
