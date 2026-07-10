//! Pinned TOON v3.0 structured-payload encoding.
//!
//! The bundled implementation is deliberately unavailable for prose: callers
//! must provide a JSON object or array. The exact upstream snapshot and vectors
//! are vendored under `spec/toon` and exercised by tests in this module.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use toon_format::types::{DecodeOptions, EncodeOptions, KeyFoldingMode, PathExpansionMode};

/// One-line notation note prepended only to the first TOON prompt payload.
pub const TOON_FORMAT_NOTE: &str =
    "TOON is indentation-based structured data; array headers declare [count]{fields}.";

/// Safe TOON wrapper errors with no provider or secret material.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ToonError {
    #[error("TOON is reserved for structured object/array payloads, not prose or primitives")]
    RequiresStructuredRoot,
    #[error("TOON encode failed: {0}")]
    Encode(String),
    #[error("TOON decode failed: {0}")]
    Decode(String),
}

/// Result ready to place in a model prompt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EncodedToon {
    /// Raw encoded payload without the explanatory note.
    pub payload: String,
    /// First payload includes the note; subsequent payloads equal `payload`.
    pub prompt_text: String,
    pub emitted_format_note: bool,
}

/// Session-local state enforcing the one-time format note.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ToonPromptEncoder {
    format_note_emitted: bool,
}

impl ToonPromptEncoder {
    /// Encodes structured JSON, prepending the note only on the first success.
    ///
    /// # Errors
    ///
    /// Rejects primitive roots and invalid/non-encodable values.
    pub fn encode(&mut self, value: &Value) -> Result<EncodedToon, ToonError> {
        let payload = encode(value)?;
        let emitted_format_note = !self.format_note_emitted;
        let prompt_text = if emitted_format_note {
            format!("{TOON_FORMAT_NOTE}\n{payload}")
        } else {
            payload.clone()
        };
        self.format_note_emitted = true;
        Ok(EncodedToon {
            payload,
            prompt_text,
            emitted_format_note,
        })
    }

    /// Resets the note state, intended for a new provider conversation.
    pub fn reset(&mut self) {
        self.format_note_emitted = false;
    }
}

/// Encodes a JSON object or array using the pinned v3.0 behavior.
///
/// # Errors
///
/// Rejects primitive roots or an encoder failure.
pub fn encode(value: &Value) -> Result<String, ToonError> {
    ensure_structured(value)?;
    // The vendored v3.0 default vectors use unfolded keys. Keeping that exact
    // representation also makes the decoder independent of path expansion.
    let options = EncodeOptions::new().with_key_folding(KeyFoldingMode::Off);
    let encoded = toon_format::encode(value, &options)
        .map_err(|error| ToonError::Encode(error.to_string()))?;
    if round_trips(&encoded, value) {
        Ok(encoded)
    } else {
        Err(ToonError::Encode(
            "self-validation failed for the pinned v3.0 encoding".to_owned(),
        ))
    }
}

/// Decodes a TOON document and safely expands encoder-folded identifier paths.
///
/// # Errors
///
/// Rejects malformed TOON and primitive roots.
pub fn decode(input: &str) -> Result<Value, ToonError> {
    let options = DecodeOptions::new().with_expand_paths(PathExpansionMode::Off);
    let value = toon_format::decode(input, &options)
        .map_err(|error| ToonError::Decode(error.to_string()))?;
    ensure_structured(&value)?;
    Ok(value)
}

fn ensure_structured(value: &Value) -> Result<(), ToonError> {
    if value.is_object() || value.is_array() {
        Ok(())
    } else {
        Err(ToonError::RequiresStructuredRoot)
    }
}

fn round_trips(encoded: &str, expected: &Value) -> bool {
    decode(encoded).is_ok_and(|decoded| decoded == *expected)
}

#[cfg(test)]
mod tests {
    use proptest::{collection, prelude::*};
    use serde_json::{Map, Number, Value, json};

    use super::{TOON_FORMAT_NOTE, ToonPromptEncoder, decode, encode};

    fn json_leaf() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i32>().prop_map(|value| Value::Number(Number::from(value))),
            "[a-zA-Z0-9 _./:@-]{0,32}".prop_map(Value::String),
        ]
    }

    fn structured_json() -> impl Strategy<Value = Value> {
        let primitive_array = collection::vec(json_leaf(), 0..8).prop_map(Value::Array);
        let object_value =
            prop_oneof![json_leaf(), primitive_array].prop_recursive(4, 64, 8, |inner| {
                collection::btree_map("[a-zA-Z_][a-zA-Z0-9_]{0,12}", inner, 0..8)
                    .prop_map(|values| Value::Object(values.into_iter().collect::<Map<_, _>>()))
            });
        prop_oneof![
            collection::btree_map("[a-zA-Z_][a-zA-Z0-9_]{0,12}", object_value, 0..8)
                .prop_map(|values| Value::Object(values.into_iter().collect::<Map<_, _>>())),
            collection::vec(json_leaf(), 0..16).prop_map(Value::Array),
        ]
    }

    proptest! {
        #[test]
        fn valid_generated_structures_must_encode_and_round_trip(value in structured_json()) {
            let encoded = encode(&value)?;
            let decoded = decode(&encoded)?;
            prop_assert_eq!(decoded, value);
        }

        #[test]
        fn uniform_record_tables_round_trip(
            rows in collection::vec((any::<i32>(), "[a-zA-Z0-9 _-]{0,24}", any::<bool>()), 0..20)
        ) {
            let value = json!({
                "rows": rows.into_iter().map(|(id, name, active)| {
                    json!({"id": id, "name": name, "active": active})
                }).collect::<Vec<_>>()
            });
            let encoded = encode(&value)?;
            let decoded = decode(&encoded)?;
            prop_assert_eq!(decoded, value);
        }
    }

    #[test]
    fn official_users_reference_vector_matches() {
        let json_input = include_str!("../spec/toon/vectors/users.json");
        let expected = include_str!("../spec/toon/vectors/users.toon").trim_end();
        let value: Value = serde_json::from_str(json_input).unwrap_or(Value::Null);
        assert_eq!(encode(&value).as_deref(), Ok(expected));
        assert_eq!(decode(expected), Ok(value));
    }

    #[test]
    fn all_vendored_reference_vectors_match() {
        let fixture: Value =
            serde_json::from_str(include_str!("../spec/toon/vectors/arrays-tabular.json"))
                .unwrap_or(Value::Null);
        let cases = fixture["tests"].as_array().cloned().unwrap_or_default();
        assert_eq!(cases.len(), 5);
        for case in cases {
            let input = &case["input"];
            let expected = case["expected"].as_str().unwrap_or_default();
            assert_eq!(encode(input).as_deref(), Ok(expected));
            assert_eq!(decode(expected), Ok(input.clone()));
        }
        let source = include_str!("../spec/toon/SOURCE.toml");
        assert!(source.contains("spec_version = \"3.0\""));
        assert!(source.contains("commit = \"c09f73b267323190f61de5b91563fa579b3b7c5e\""));
        assert!(source.contains("complete_spec = \"SPEC.md\""));
        assert!(source.contains("fixture_manifest = \"MANIFEST.sha256\""));
    }

    #[test]
    fn complete_official_v3_spec_and_fixture_suite_are_vendored()
    -> Result<(), Box<dyn std::error::Error>> {
        const FIXTURES: &[&str] = &[
            include_str!("../spec/toon/tests/fixtures/decode/arrays-nested.json"),
            include_str!("../spec/toon/tests/fixtures/decode/arrays-primitive.json"),
            include_str!("../spec/toon/tests/fixtures/decode/arrays-tabular.json"),
            include_str!("../spec/toon/tests/fixtures/decode/blank-lines.json"),
            include_str!("../spec/toon/tests/fixtures/decode/delimiters.json"),
            include_str!("../spec/toon/tests/fixtures/decode/indentation-errors.json"),
            include_str!("../spec/toon/tests/fixtures/decode/numbers.json"),
            include_str!("../spec/toon/tests/fixtures/decode/objects.json"),
            include_str!("../spec/toon/tests/fixtures/decode/path-expansion.json"),
            include_str!("../spec/toon/tests/fixtures/decode/primitives.json"),
            include_str!("../spec/toon/tests/fixtures/decode/root-form.json"),
            include_str!("../spec/toon/tests/fixtures/decode/validation-errors.json"),
            include_str!("../spec/toon/tests/fixtures/decode/whitespace.json"),
            include_str!("../spec/toon/tests/fixtures/encode/arrays-nested.json"),
            include_str!("../spec/toon/tests/fixtures/encode/arrays-objects.json"),
            include_str!("../spec/toon/tests/fixtures/encode/arrays-primitive.json"),
            include_str!("../spec/toon/tests/fixtures/encode/arrays-tabular.json"),
            include_str!("../spec/toon/tests/fixtures/encode/delimiters.json"),
            include_str!("../spec/toon/tests/fixtures/encode/key-folding.json"),
            include_str!("../spec/toon/tests/fixtures/encode/objects.json"),
            include_str!("../spec/toon/tests/fixtures/encode/primitives.json"),
            include_str!("../spec/toon/tests/fixtures/encode/whitespace.json"),
        ];
        let spec = include_str!("../spec/toon/SPEC.md");
        let manifest = include_str!("../spec/toon/MANIFEST.sha256");
        assert!(spec.starts_with("# TOON Specification"));
        assert!(spec.contains("**Version:** 3.0"));
        assert!(spec.len() > 60_000);
        assert_eq!(manifest.lines().count(), 29);
        assert_eq!(FIXTURES.len(), 22);
        for fixture in FIXTURES {
            let value: Value = serde_json::from_str(fixture)?;
            assert!(
                value["tests"]
                    .as_array()
                    .is_some_and(|tests| !tests.is_empty())
            );
        }
        Ok(())
    }

    #[test]
    fn wrapper_matches_all_applicable_default_official_encode_vectors()
    -> Result<(), Box<dyn std::error::Error>> {
        const ENCODE_FIXTURES: &[&str] = &[
            include_str!("../spec/toon/tests/fixtures/encode/arrays-nested.json"),
            include_str!("../spec/toon/tests/fixtures/encode/arrays-objects.json"),
            include_str!("../spec/toon/tests/fixtures/encode/arrays-primitive.json"),
            include_str!("../spec/toon/tests/fixtures/encode/arrays-tabular.json"),
            include_str!("../spec/toon/tests/fixtures/encode/objects.json"),
            include_str!("../spec/toon/tests/fixtures/encode/primitives.json"),
            include_str!("../spec/toon/tests/fixtures/encode/whitespace.json"),
        ];
        let mut checked = 0_usize;
        for fixture in ENCODE_FIXTURES {
            let value: Value = serde_json::from_str(fixture)?;
            for case in value["tests"].as_array().into_iter().flatten() {
                let options_are_default = case
                    .get("options")
                    .is_none_or(|options| options.as_object().is_some_and(Map::is_empty));
                let input = &case["input"];
                if options_are_default && (input.is_object() || input.is_array()) {
                    let expected = case["expected"].as_str().ok_or_else(|| {
                        std::io::Error::other(
                            "official encode fixture expected output must be text",
                        )
                    })?;
                    assert_eq!(
                        encode(input).as_deref(),
                        Ok(expected),
                        "official vector failed: {}",
                        case["name"].as_str().unwrap_or("unnamed")
                    );
                    checked = checked.saturating_add(1);
                }
            }
        }
        assert!(checked >= 60, "expected broad official vector coverage");
        Ok(())
    }

    #[test]
    fn first_prompt_payload_alone_has_format_note() {
        let mut encoder = ToonPromptEncoder::default();
        let first = encoder.encode(&json!({"rows": [1, 2]}));
        let second = encoder.encode(&json!({"rows": [3, 4]}));
        assert!(first.is_ok_and(|value| {
            value.emitted_format_note && value.prompt_text.starts_with(TOON_FORMAT_NOTE)
        }));
        assert!(second.is_ok_and(|value| {
            !value.emitted_format_note && !value.prompt_text.contains(TOON_FORMAT_NOTE)
        }));
    }

    #[test]
    fn prose_is_rejected() {
        assert!(encode(&json!("write a long essay")).is_err());
    }
}
