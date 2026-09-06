//! Child progress is a bounded preview or an invalidation of its canonical source.
use super::{MAX_SUBAGENT_PROGRESS_BYTES, OrchestrationError};
use rw_types::json_encoding::JsonWriter;
use serde::Serialize;
use serde_json::Value;

fn fits(event: &impl Serialize) -> Result<bool, OrchestrationError> {
    let mut writer = JsonWriter::count(MAX_SUBAGENT_PROGRESS_BYTES);
    match writer.serialize(event) {
        Ok(()) => Ok(true),
        Err(_) if writer.exceeded() => Ok(false),
        Err(error) => Err(OrchestrationError::Observer(error.to_string())),
    }
}
fn invalidation(sequence: Option<u64>) -> Result<Value, OrchestrationError> {
    sequence.map(|_| Value::Null).ok_or_else(|| {
        OrchestrationError::Observer(
            "child progress invalidation requires a canonical sequence".into(),
        )
    })
}
pub(super) fn encode(
    sequence: Option<u64>,
    event: &impl Serialize,
) -> Result<Value, OrchestrationError> {
    if fits(event)? {
        let value = serde_json::to_value(event)
            .map_err(|error| OrchestrationError::Observer(error.to_string()))?;
        if value.is_null() {
            invalidation(sequence)
        } else {
            Ok(value)
        }
    } else {
        invalidation(sequence)
    }
}
pub(crate) fn admit(sequence: Option<u64>, event: Value) -> Result<Value, OrchestrationError> {
    if event.is_null() || !fits(&event)? {
        invalidation(sequence)
    } else {
        Ok(event)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn oversized_canonical_progress_is_an_invalidation() {
        let text = "x".repeat(MAX_SUBAGENT_PROGRESS_BYTES + 1);
        assert_eq!(
            encode(Some(10), &text).expect("source invalidation"),
            Value::Null
        );
        assert!(encode(None, &text).is_err());
        assert_eq!(
            admit(Some(10), Value::String(text)).expect("source invalidation"),
            Value::Null
        );
        assert!(admit(None, Value::Null).is_err());
    }
    #[test]
    fn preview_limit_counts_json_escaping_without_allocating_the_encoded_body() {
        let text = "\n".repeat(MAX_SUBAGENT_PROGRESS_BYTES / 2);
        assert!(
            encode(Some(3), &text)
                .expect("source invalidation")
                .is_null()
        );
        assert_eq!(
            encode(Some(3), &"small").expect("preview"),
            Value::String("small".into())
        );
    }
}
