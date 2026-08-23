//! Wire data and structural limits for the signed update protocol.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::UpdateChannel;

/// Version of root and release metadata accepted by this protocol.
pub const UPDATE_SCHEMA_VERSION: u16 = 1;
/// Domain prefix for every root and release metadata signature.
pub const UPDATE_SIGNATURE_DOMAIN: &[u8] = b"rottweiler-update-metadata-v1\0";
/// Serialized role name for root metadata.
pub const UPDATE_ROOT_ROLE: &str = "root";
/// Serialized role name for release metadata.
pub const UPDATE_RELEASE_ROLE: &str = "release";

/// Largest serialized signed metadata envelope.
pub const MAX_UPDATE_ENVELOPE_BYTES: usize = 1024 * 1024;
/// Largest decoded signed metadata payload.
pub const MAX_UPDATE_PAYLOAD_BYTES: usize = 768 * 1024;
/// Largest number of signatures carried by one envelope.
pub const MAX_UPDATE_SIGNATURES: usize = 32;
/// Largest key set or role-key-id set in a root payload.
pub const MAX_UPDATE_KEYS: usize = 32;
/// Largest platform target map in one release payload.
pub const MAX_UPDATE_TARGETS: usize = 32;
/// Largest UTF-8 release-notes field in one release payload.
pub const MAX_UPDATE_RELEASE_NOTES_BYTES: usize = 64 * 1024;
/// Largest sequential root chain accepted or persisted by the updater.
pub const MAX_UPDATE_ROOT_CHAIN_ENTRIES: usize = 16;
/// Largest key id or platform selector.
pub const MAX_UPDATE_SELECTOR_BYTES: usize = 128;

/// Largest compressed release artifact that may be signed, verified, or downloaded.
pub const MAX_UPDATE_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// Signed envelope containing exact base64 payload bytes and detached signatures.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedEnvelope {
    pub payload: String,
    pub signatures: Vec<MetadataSignature>,
}

/// One key-id-qualified base64 Ed25519 signature.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataSignature {
    pub key_id: String,
    pub signature: String,
}

/// Signed root-role payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootMetadata {
    pub schema_version: u16,
    pub role: String,
    pub version: u64,
    pub expires_unix: u64,
    pub keys: BTreeMap<String, String>,
    pub root_key_ids: Vec<String>,
    pub root_threshold: usize,
    pub release_key_ids: Vec<String>,
    pub release_threshold: usize,
}

/// Signed release-role payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMetadata {
    pub schema_version: u16,
    pub role: String,
    pub version: u64,
    pub expires_unix: u64,
    pub channel: UpdateChannel,
    pub release_notes: String,
    pub targets: BTreeMap<String, ReleaseTarget>,
}

/// One platform artifact selected by signed release metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseTarget {
    pub version: String,
    pub url: String,
    pub length: u64,
    pub sha256: String,
}

/// Public root-chain document published beside release metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootChainDocument {
    pub roots: Vec<RootChainEntry>,
}

/// One exact signed root envelope in a sequential root chain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootChainEntry {
    pub version: u64,
    pub envelope: String,
}

/// Builds the exact bytes signed and verified for one metadata role.
#[must_use]
pub fn signature_message(role: &str, payload: &[u8]) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(UPDATE_SIGNATURE_DOMAIN.len() + role.len() + payload.len() + 1);
    message.extend_from_slice(UPDATE_SIGNATURE_DOMAIN);
    message.extend_from_slice(role.as_bytes());
    message.push(0);
    message.extend_from_slice(payload);
    message
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{
        MetadataSignature, ReleaseMetadata, ReleaseTarget, RootChainDocument, RootChainEntry,
        SignedEnvelope, UPDATE_SCHEMA_VERSION, UPDATE_SIGNATURE_DOMAIN, signature_message,
    };
    use crate::config::UpdateChannel;

    #[test]
    fn shared_dtos_preserve_the_signed_json_shape() {
        let release = ReleaseMetadata {
            schema_version: UPDATE_SCHEMA_VERSION,
            role: "release".to_owned(),
            version: 7,
            expires_unix: 1_900_000_000,
            channel: UpdateChannel::Stable,
            release_notes: "notes".to_owned(),
            targets: BTreeMap::from([(
                "darwin-arm64".to_owned(),
                ReleaseTarget {
                    version: "1.2.3".to_owned(),
                    url: "https://updates.example/rottweiler-1.2.3-darwin-arm64.tar.gz".to_owned(),
                    length: 42,
                    sha256: "00".repeat(32),
                },
            )]),
        };
        let payload = serde_json::to_vec(&release).expect("serialize release");
        let envelope = SignedEnvelope {
            payload: "encoded".to_owned(),
            signatures: vec![MetadataSignature {
                key_id: "release-1".to_owned(),
                signature: "signature".to_owned(),
            }],
        };
        let chain = RootChainDocument {
            roots: vec![RootChainEntry {
                version: 1,
                envelope: "root".to_owned(),
            }],
        };

        assert_eq!(
            serde_json::from_slice::<ReleaseMetadata>(&payload).expect("parse release"),
            release
        );
        assert_eq!(
            serde_json::to_value(envelope).expect("serialize envelope"),
            json!({
                "payload": "encoded",
                "signatures": [{"key_id": "release-1", "signature": "signature"}],
            })
        );
        assert_eq!(
            serde_json::to_value(chain).expect("serialize root chain"),
            json!({"roots": [{"version": 1, "envelope": "root"}]})
        );
    }

    #[test]
    fn signature_message_has_one_shared_domain_framing() {
        let payload = br#"{"role":"root"}"#;
        let message = signature_message("root", payload);
        let mut expected = UPDATE_SIGNATURE_DOMAIN.to_vec();
        expected.extend_from_slice(b"root\0");
        expected.extend_from_slice(payload);
        assert_eq!(message, expected);
    }

    #[test]
    fn wire_dtos_reject_unknown_fields() {
        let malformed = br#"{"payload":"encoded","signatures":[],"extra":true}"#;
        assert!(serde_json::from_slice::<SignedEnvelope>(malformed).is_err());
    }
}
