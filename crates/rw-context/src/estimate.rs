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

    /// Estimates tokens using the canonical JSON byte length.
    #[must_use]
    pub fn value(value: &Value) -> u64 {
        let mut counter = JsonByteCounter::default();
        // Key order changes the bytes, but not their length. Counting the same
        // serializer's output avoids cloning the value and allocating its JSON.
        serde_json::to_writer(&mut counter, value).map_or(u64::MAX, |()| counter.bytes.div_ceil(4))
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

#[derive(Default)]
struct JsonByteCounter {
    bytes: u64,
}

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(usize_to_u64(bytes.len()));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Returns a recursively key-sorted JSON value for stable hashing and sizing.
#[must_use]
pub fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_unstable_by_key(|(path, _)| *path);
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
    use proptest::prelude::*;
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

    #[test]
    fn counted_json_matches_canonical_serialization() -> Result<(), Box<dyn std::error::Error>> {
        let values = [
            json!(null),
            json!(true),
            json!([0, -1, u64::MAX, i64::MIN, 1.25, 1e-100]),
            json!({"z": ["\u{0}\n\r\t\"\\", "é🙂"], "a": {"x": false}}),
            json!({"large": "\"\n🙂".repeat(256 * 1024)}),
        ];
        for value in values {
            let canonical = serde_json::to_vec(&canonicalize_json(&value))?;
            assert_eq!(
                LocalTokenEstimator::value(&value),
                u64::try_from(canonical.len())?.div_ceil(4)
            );
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn counted_json_preserves_estimates_for_nested_values(value in json_value()) {
            let expected = serde_json::to_vec(&canonicalize_json(&value))
                .ok()
                .map(|bytes| super::usize_to_u64(bytes.len()).div_ceil(4));
            prop_assert_eq!(Some(LocalTokenEstimator::value(&value)), expected);
        }
    }

    fn json_value() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::from),
            any::<i64>().prop_map(serde_json::Value::from),
            ".{0,64}".prop_map(serde_json::Value::from),
        ]
        .prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..8).prop_map(serde_json::Value::Array),
                proptest::collection::btree_map(".{0,16}", inner, 0..8)
                    .prop_map(|entries| serde_json::Value::Object(entries.into_iter().collect())),
            ]
        })
    }
}
