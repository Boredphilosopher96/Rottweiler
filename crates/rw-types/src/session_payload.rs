//! Durable session payload identities carried by canonical tool completion events.
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use ts_rs::TS;

pub const MAX_SESSION_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_TOOL_PAYLOADS: usize = 8;
pub const MAX_PAYLOAD_WINDOW_BYTES: usize = 192 * 1024;
pub const MAX_PAYLOAD_QUERY_BYTES: usize = 512;

/// The digest binds the payload's length and immutable chunk checksums.
/// A reference resolves only within its owning session's durable payload directory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionPayloadReference {
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub digest: String,
    #[schemars(range(max = MAX_SESSION_PAYLOAD_BYTES))]
    pub bytes: usize,
}

impl SessionPayloadReference {
    /// # Errors
    /// Rejects malformed digests and payloads outside the source byte limit.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.bytes > MAX_SESSION_PAYLOAD_BYTES
        {
            return Err("invalid session payload identity");
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SessionPayloadReference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            digest: String,
            bytes: usize,
        }
        let fields = Fields::deserialize(deserializer)?;
        let reference = Self {
            digest: fields.digest,
            bytes: fields.bytes,
        };
        reference.validate().map_err(de::Error::custom)?;
        Ok(reference)
    }
}

pub(crate) fn deserialize_references<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SessionPayloadReference>, D::Error> {
    struct References;
    impl<'de> de::Visitor<'de> for References {
        type Value = Vec<SessionPayloadReference>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("at most eight session payload references")
        }
        fn visit_seq<A: de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut references = Vec::new();
            while let Some(reference) = sequence.next_element()? {
                if references.len() == MAX_TOOL_PAYLOADS {
                    return Err(de::Error::custom("tool payload reference limit exceeded"));
                }
                references.push(reference);
            }
            Ok(references)
        }
    }
    deserializer.deserialize_seq(References)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Deserialize)]
    struct Completion {
        #[serde(deserialize_with = "deserialize_references")]
        payloads: Vec<SessionPayloadReference>,
    }
    #[test]
    fn completion_requires_a_bounded_typed_attachment_list() -> Result<(), serde_json::Error> {
        assert!(serde_json::from_str::<Completion>("{}").is_err());
        let reference = serde_json::json!({"digest":"a".repeat(64),"bytes":1});
        let valid: Completion = serde_json::from_value(
            serde_json::json!({"payloads":vec![reference.clone();MAX_TOOL_PAYLOADS]}),
        )?;
        assert_eq!(valid.payloads.len(), MAX_TOOL_PAYLOADS);
        assert!(
            serde_json::from_value::<Completion>(
                serde_json::json!({"payloads":vec![reference;MAX_TOOL_PAYLOADS+1]})
            )
            .is_err()
        );
        for invalid in [
            serde_json::json!({"digest":"A".repeat(64),"bytes":1}),
            serde_json::json!({"digest":"0".repeat(65),"bytes":1}),
            serde_json::json!({"digest":"0".repeat(64),"bytes":MAX_SESSION_PAYLOAD_BYTES+1}),
            serde_json::json!({"digest":"0".repeat(64),"bytes":1,"path":"untrusted"}),
        ] {
            assert!(serde_json::from_value::<SessionPayloadReference>(invalid).is_err());
        }
        Ok(())
    }
}
