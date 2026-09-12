//! Host-minted identities bind callbacks to one admitted extension invocation.
use rw_memory_derive::PrepareAllocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema, TS, PrepareAllocation)]
#[serde(transparent)]
pub struct ExtensionInvocationId(
    #[schemars(length(min = 32, max = 32), regex(pattern = "^[0-9a-f]{32}$"))] String,
);
impl ExtensionInvocationId {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut value = String::with_capacity(32);
        for byte in bytes {
            value.push(char::from(HEX[usize::from(byte >> 4)]));
            value.push(char::from(HEX[usize::from(byte & 15)]));
        }
        Self(value)
    }
}
impl<'de> Deserialize<'de> for ExtensionInvocationId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(serde::de::Error::custom(
                "invalid extension invocation identity",
            ));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS, PrepareAllocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionControlRequest {
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<ExtensionInvocationId>")]
    pub origin: Option<ExtensionInvocationId>,
    pub control: crate::extension_control::ExtensionControl,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{ExtensionControlRequest, ExtensionInvocationId};
    #[test]
    fn identities_and_required_control_origin_are_closed() {
        let identity = ExtensionInvocationId::from_bytes([0xab; 16]);
        let wire = serde_json::to_value(&identity).expect("identity wire");
        assert_eq!(
            serde_json::from_value::<ExtensionInvocationId>(wire).expect("identity wire"),
            identity
        );
        for value in [
            "",
            "ab",
            "ABABABABABABABABABABABABABABABAB",
            "z".repeat(32).as_str(),
        ] {
            assert!(
                serde_json::from_value::<ExtensionInvocationId>(serde_json::json!(value)).is_err()
            );
        }
        let control = serde_json::json!({"action":"select_mode","mode":"plan"});
        assert!(
            serde_json::from_value::<ExtensionControlRequest>(
                serde_json::json!({"control":control})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ExtensionControlRequest>(
                serde_json::json!({"origin":null,"control":control})
            )
            .is_ok()
        );
        assert!(
            serde_json::from_value::<ExtensionControlRequest>(
                serde_json::json!({"origin":null,"control":control,"session_id":"foreign"})
            )
            .is_err()
        );
    }
}
