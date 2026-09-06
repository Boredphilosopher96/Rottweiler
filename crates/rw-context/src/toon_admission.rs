//! Allocation planning for the pinned TOON encoder and round-trip validator.
use serde_json::Value;
use std::io::Write;

/// The encoder owns normalized JSON, encoded text, a parse layout and a decoded
/// validation tree concurrently. Object/array metadata is charged per node; text
/// is charged separately so one large scalar does not imply thousands of nodes.
#[derive(Clone, Copy, Debug)]
pub struct ToonAllocation {
    pub working_bytes: usize,
    pub prompt_bytes: usize,
}
impl ToonAllocation {
    /// Inspect borrowed input before any encoder/decoder allocation is started.
    #[must_use]
    pub fn for_value(value: &Value) -> Option<Self> {
        if !value.is_object() && !value.is_array() {
            return None;
        }
        let mut shape = Shape::default();
        shape.visit(value, 0)?;
        let mut bytes = Count(0);
        serde_json::to_writer(&mut bytes, value).ok()?;
        // TOON keys/strings never escape to more bytes than JSON. Headers,
        // separators and nesting indentation are bounded independently.
        let prompt_bytes = bytes
            .0
            .checked_add(shape.indentation)?
            .checked_add(shape.nodes.checked_mul(64)?)?
            .checked_add(super::TOON_FORMAT_NOTE.len() + 1)?;
        // Includes geometric output capacities, the pinned encoder's JSON
        // normalization copies, scanner/layout strings and decoded validation.
        // 8KiB/node covers both map tables and parser node bookkeeping.
        let working_bytes = prompt_bytes
            .checked_mul(16)?
            .checked_add(shape.nodes.checked_mul(8192)?)?;
        Some(Self {
            working_bytes,
            prompt_bytes,
        })
    }
}
#[derive(Default)]
struct Shape {
    nodes: usize,
    indentation: usize,
}
impl Shape {
    fn visit(&mut self, value: &Value, depth: usize) -> Option<()> {
        if depth > 64 {
            return None;
        }
        self.nodes = self.nodes.checked_add(1)?;
        self.indentation = self.indentation.checked_add(depth.checked_mul(2)?)?;
        match value {
            Value::Array(values) => {
                for value in values {
                    self.visit(value, depth + 1)?;
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    self.visit(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Some(())
    }
}
struct Count(usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("TOON allocation overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::ToonAllocation;
    #[test]
    fn plan_covers_escaped_and_indented_output_before_encode() {
        for value in [
            serde_json::json!({"rows":(0..100).map(|index| serde_json::json!({"id":index,"text":"🙂\\\"\n".repeat(100)})).collect::<Vec<_>>()}),
            serde_json::json!({"nested":{"list":[[1,2],[3,4],{"x":"x"}]}}),
        ] {
            let plan = ToonAllocation::for_value(&value).expect("plan");
            let encoded = crate::ToonPromptEncoder::default()
                .encode_bounded(&value, plan.working_bytes)
                .expect("admitted encoding");
            assert!(encoded.prompt_text.len() <= plan.prompt_bytes);
            assert!(
                crate::ToonPromptEncoder::default()
                    .encode_bounded(&value, plan.working_bytes - 1)
                    .is_err()
            );
        }
    }
}
