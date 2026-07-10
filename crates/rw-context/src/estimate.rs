//! Deterministic, provider-independent token estimation.

use rw_providers::ToolDefinition;
use rw_types::{Block, ToolOutput, ToolOutputPart, Turn};
use serde_json::{Map, Value};

/// A cheap local estimator used before a provider reports authoritative usage.
///
/// The estimator intentionally uses no provider tokenizer. It approximates one
/// token per four UTF-8 bytes and accounts for message and block framing. The
/// [`crate::Budgeter`] reconciles this estimate against live provider usage.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalTokenEstimator;

impl LocalTokenEstimator {
    /// Estimates tokens for plain text without message framing.
    #[must_use]
    pub fn text(text: &str) -> u64 {
        usize_to_u64(text.len()).div_ceil(4)
    }

    /// Estimates tokens for a JSON value after canonicalizing object keys.
    #[must_use]
    pub fn value(value: &Value) -> u64 {
        let bytes = serde_json::to_vec(&canonicalize_json(value)).unwrap_or_default();
        usize_to_u64(bytes.len()).div_ceil(4)
    }

    /// Estimates one provider-neutral conversation turn.
    #[must_use]
    pub fn turn(turn: &Turn) -> u64 {
        turn.blocks.iter().fold(4_u64, |total, block| {
            total.saturating_add(Self::block(block))
        })
    }

    /// Estimates all tool definitions, including provider framing.
    #[must_use]
    pub fn tools(tools: &[ToolDefinition]) -> u64 {
        tools.iter().fold(0_u64, |total, tool| {
            let estimate = 12_u64
                .saturating_add(Self::text(&tool.name))
                .saturating_add(Self::text(&tool.description))
                .saturating_add(Self::value(&tool.input_schema));
            total.saturating_add(estimate)
        })
    }

    fn block(block: &Block) -> u64 {
        let body = match block {
            Block::Text { text } => Self::text(text),
            Block::Thinking { content, signature } => {
                Self::text(content).saturating_add(signature.as_deref().map_or(0, Self::text))
            }
            Block::ToolCall { id, name, args } => Self::text(&id.0)
                .saturating_add(Self::text(name))
                .saturating_add(Self::value(args)),
            Block::ToolResult { id, output, .. } => {
                Self::text(&id.0).saturating_add(Self::tool_output(output))
            }
            Block::Image { media_type, data } => Self::text(media_type).saturating_add(
                Self::value(&serde_json::to_value(data).unwrap_or(Value::Null)),
            ),
            Block::Citation {
                uri,
                title,
                excerpt,
            } => Self::text(uri)
                .saturating_add(title.as_deref().map_or(0, Self::text))
                .saturating_add(excerpt.as_deref().map_or(0, Self::text)),
        };
        body.saturating_add(3)
    }

    fn tool_output(output: &ToolOutput) -> u64 {
        match output {
            ToolOutput::Text { text } => Self::text(text),
            ToolOutput::Structured { value } => Self::value(value),
            ToolOutput::Mixed { parts } => parts.iter().fold(0_u64, |total, part| {
                let estimate = match part {
                    ToolOutputPart::Text { text } => Self::text(text),
                    ToolOutputPart::Structured { value } => Self::value(value),
                    ToolOutputPart::Image { media_type, data } => Self::text(media_type)
                        .saturating_add(Self::value(
                            &serde_json::to_value(data).unwrap_or(Value::Null),
                        )),
                };
                total.saturating_add(estimate)
            }),
        }
    }
}

/// Returns a recursively key-sorted JSON value for stable hashing and sizing.
#[must_use]
pub fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            let mut canonical = Map::new();
            for (key, child) in entries {
                canonical.insert(key.clone(), canonicalize_json(child));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_json).collect()),
        primitive => primitive.clone(),
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{LocalTokenEstimator, canonicalize_json};

    #[test]
    fn canonical_json_sorts_nested_keys() {
        let canonical = canonicalize_json(&json!({"z": {"b": 2, "a": 1}, "a": 0}));
        let encoded = serde_json::to_string(&canonical).ok();
        assert_eq!(encoded.as_deref(), Some(r#"{"a":0,"z":{"a":1,"b":2}}"#));
    }

    #[test]
    fn estimator_is_monotonic_for_text() {
        assert!(LocalTokenEstimator::text("four more bytes") > LocalTokenEstimator::text("four"));
    }
}
