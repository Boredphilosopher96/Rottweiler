//! TUF-style signed update metadata verification.
//!
//! Metadata envelopes carry the exact payload bytes as base64. Signatures are
//! checked over those bytes before the payload is parsed, preventing parser or
//! reserialization differences from changing what was authenticated.

use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use url::Url;

use rw_types::config::UpdateChannel;

const SIGNATURE_DOMAIN: &[u8] = b"rottweiler-update-metadata-v1\0";
const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 768 * 1024;
const MAX_SIGNATURES: usize = 32;
const MAX_KEYS: usize = 32;
const MAX_TARGETS: usize = 32;
const MAX_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RELEASE_NOTES_BYTES: usize = 64 * 1024;
const MAX_ROOT_CHAIN: usize = 16;

/// Legacy compile-time public root key id used by single-key release builds.
pub const EMBEDDED_ROOT_KEY_ID: Option<&str> = option_env!("ROTTWEILER_UPDATE_ROOT_KEY_ID");
/// Legacy compile-time base64 Ed25519 public root key used by single-key builds.
pub const EMBEDDED_ROOT_PUBLIC_KEY: Option<&str> =
    option_env!("ROTTWEILER_UPDATE_ROOT_PUBLIC_KEY_B64");
/// Compile-time JSON object of root-role key ids to base64 Ed25519 public keys.
pub const EMBEDDED_ROOT_KEYS_JSON: Option<&str> = option_env!("ROTTWEILER_UPDATE_ROOT_KEYS_JSON");
/// Compile-time root threshold paired with the embedded root-role keys.
pub const EMBEDDED_ROOT_THRESHOLD: Option<&str> = option_env!("ROTTWEILER_UPDATE_ROOT_THRESHOLD");
/// Compile-time positive root version paired with the embedded public keys.
pub const EMBEDDED_ROOT_VERSION: Option<&str> = option_env!("ROTTWEILER_UPDATE_ROOT_VERSION");

/// Sanitized signed-update failure.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum UpdateVerificationError {
    /// Production build omitted its trust anchor.
    #[error("this build has no embedded update trust root")]
    TrustRootUnavailable,
    /// Envelope or authenticated payload exceeded its bound.
    #[error("signed update metadata exceeded its size limit")]
    MetadataTooLarge,
    /// Envelope could not be decoded safely.
    #[error("signed update envelope is malformed")]
    MalformedEnvelope,
    /// Authenticated payload could not be decoded after verification.
    #[error("signed update metadata is malformed")]
    MalformedMetadata,
    /// A role did not meet its unique-key signature threshold.
    #[error("signed update metadata did not meet its signature threshold")]
    SignatureThreshold,
    /// Root rotation skipped, replayed, or changed trust without authorization.
    #[error("signed update root rotation is invalid")]
    InvalidRootRotation,
    /// Signed metadata is expired at the fixed update start time.
    #[error("signed update metadata is expired")]
    Expired,
    /// Metadata version is below the durable high-water mark.
    #[error("signed update metadata rollback was rejected")]
    MetadataRollback,
    /// Metadata skipped a version after durable trust was established.
    #[error("signed update metadata fast-forward was rejected")]
    MetadataFastForward,
    /// Local clock moved behind the last trusted update time.
    #[error("local clock rollback was rejected")]
    ClockRollback,
    /// Signed channel does not match the selected channel.
    #[error("signed update channel does not match the selected channel")]
    ChannelMismatch,
    /// No signed target exists for the running platform.
    #[error("signed update metadata has no artifact for this platform")]
    PlatformUnavailable,
    /// Target version or its signed fields are invalid.
    #[error("signed update target is invalid")]
    InvalidTarget,
    /// Stable channel attempted to select a prerelease.
    #[error("stable update channel cannot install a prerelease")]
    StablePrerelease,
    /// Signed downgrade was not explicitly authorized.
    #[error("signed downgrade requires --allow-downgrade")]
    DowngradeRejected,
    /// Artifact bytes differ from signed length or digest.
    #[error("update artifact failed signed length or digest verification")]
    ArtifactMismatch,
}

/// Trust anchor for the currently embedded root role.
#[derive(Clone, Debug)]
pub struct TrustedRoot {
    version: u64,
    threshold: usize,
    keys: BTreeMap<String, VerifyingKey>,
    expires_unix: u64,
    release_threshold: usize,
    release_keys: BTreeMap<String, VerifyingKey>,
}

impl TrustedRoot {
    /// Builds a trust root from exact Ed25519 public-key bytes.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized key sets, invalid thresholds, duplicate ids, and
    /// malformed or weak public keys.
    pub fn from_keys<I>(
        version: u64,
        threshold: usize,
        keys: I,
    ) -> Result<Self, UpdateVerificationError>
    where
        I: IntoIterator<Item = (String, [u8; 32])>,
    {
        let mut unique_material = BTreeSet::new();
        let mut decoded_keys = BTreeMap::new();
        for (id, bytes) in keys {
            if id.is_empty() || id.len() > 128 || !unique_material.insert(bytes) {
                return Err(UpdateVerificationError::InvalidRootRotation);
            }
            let key = VerifyingKey::from_bytes(&bytes)
                .map_err(|_| UpdateVerificationError::InvalidRootRotation)?;
            if key.is_weak() || decoded_keys.insert(id, key).is_some() {
                return Err(UpdateVerificationError::InvalidRootRotation);
            }
        }
        let keys = decoded_keys;
        if version == 0
            || keys.is_empty()
            || keys.len() > MAX_KEYS
            || threshold == 0
            || threshold > keys.len()
        {
            return Err(UpdateVerificationError::InvalidRootRotation);
        }
        Ok(Self {
            version,
            threshold,
            keys,
            expires_unix: u64::MAX,
            release_threshold: 0,
            release_keys: BTreeMap::new(),
        })
    }

    /// Loads the production trust anchor injected at compile time.
    ///
    /// # Errors
    ///
    /// Returns `TrustRootUnavailable` for normal development builds without a
    /// release key, or a sanitized root error for malformed build inputs.
    pub fn embedded() -> Result<Self, UpdateVerificationError> {
        let version = EMBEDDED_ROOT_VERSION
            .ok_or(UpdateVerificationError::TrustRootUnavailable)?
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or(UpdateVerificationError::TrustRootUnavailable)?;
        match (EMBEDDED_ROOT_KEYS_JSON, EMBEDDED_ROOT_THRESHOLD) {
            (Some(encoded_keys), Some(encoded_threshold)) => {
                if encoded_keys.len() > 16 * 1024 {
                    return Err(UpdateVerificationError::InvalidRootRotation);
                }
                let keys: BTreeMap<String, String> = serde_json::from_str(encoded_keys)
                    .map_err(|_| UpdateVerificationError::InvalidRootRotation)?;
                let threshold = encoded_threshold
                    .parse::<usize>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or(UpdateVerificationError::InvalidRootRotation)?;
                let decoded = decode_key_map(&keys)?;
                Self::from_keys(
                    version,
                    threshold,
                    decoded.into_iter().map(|(id, key)| (id, key.to_bytes())),
                )
            }
            (None, None) => {
                let id =
                    EMBEDDED_ROOT_KEY_ID.ok_or(UpdateVerificationError::TrustRootUnavailable)?;
                let encoded = EMBEDDED_ROOT_PUBLIC_KEY
                    .ok_or(UpdateVerificationError::TrustRootUnavailable)?;
                let bytes = decode_public_key(encoded)?;
                Self::from_keys(version, 1, [(id.to_owned(), bytes)])
            }
            _ => Err(UpdateVerificationError::TrustRootUnavailable),
        }
    }

    /// Authenticated root version.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

/// Durable rollback state supplied to metadata verification.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UpdateHighWaterMark {
    /// Highest authenticated root version previously accepted.
    pub root_version: u64,
    /// Highest authenticated release-metadata version previously accepted.
    pub metadata_version: u64,
    /// Last trusted wall-clock second. Earlier clocks fail closed.
    pub trusted_unix_time: u64,
}

/// Verification inputs fixed at update start.
#[derive(Clone, Debug)]
pub struct UpdateVerificationPolicy<'a> {
    /// Selected effective user channel.
    pub channel: UpdateChannel,
    /// Exact release target name for the running build.
    pub platform: &'a str,
    /// Running semantic version.
    pub current_version: &'a str,
    /// Fixed wall-clock time for every expiry decision in this update.
    pub now_unix: u64,
    /// Durable rollback/freeze high-water mark.
    pub high_water: UpdateHighWaterMark,
    /// Allows only a still-authenticated lower product version.
    pub allow_downgrade: bool,
}

/// Authenticated artifact selection. Construction is possible only after root,
/// release-role, expiry, rollback, channel, platform, and version validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedUpdate {
    root_version: u64,
    metadata_version: u64,
    version: Version,
    platform: String,
    artifact_url: Url,
    artifact_length: u64,
    artifact_sha256: String,
    release_notes: String,
    trusted_unix_time: u64,
}

impl VerifiedUpdate {
    /// Accepted root version.
    #[must_use]
    pub const fn root_version(&self) -> u64 {
        self.root_version
    }

    /// Accepted release-metadata version.
    #[must_use]
    pub const fn metadata_version(&self) -> u64 {
        self.metadata_version
    }

    /// Authenticated product version.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Authenticated platform selector.
    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// Authenticated artifact URL. Debug/error surfaces must not print it.
    #[must_use]
    pub const fn artifact_url(&self) -> &Url {
        &self.artifact_url
    }

    /// Authenticated compressed artifact length.
    #[must_use]
    pub const fn artifact_length(&self) -> u64 {
        self.artifact_length
    }

    /// Authenticated lowercase SHA-256 digest.
    #[must_use]
    pub fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }

    /// Authenticated bounded release notes.
    #[must_use]
    pub fn release_notes(&self) -> &str {
        &self.release_notes
    }

    /// High-water clock time to persist after installation.
    #[must_use]
    pub const fn trusted_unix_time(&self) -> u64 {
        self.trusted_unix_time
    }

    /// Verifies exact downloaded artifact bytes before parsing or extraction.
    ///
    /// # Errors
    ///
    /// Rejects any length or SHA-256 mismatch.
    pub fn verify_artifact(&self, bytes: &[u8]) -> Result<(), UpdateVerificationError> {
        if u64::try_from(bytes.len()).ok() != Some(self.artifact_length)
            || sha256_hex(bytes) != self.artifact_sha256
        {
            return Err(UpdateVerificationError::ArtifactMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SignedEnvelope {
    payload: String,
    signatures: Vec<MetadataSignature>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MetadataSignature {
    key_id: String,
    signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RootMetadata {
    schema_version: u16,
    role: String,
    version: u64,
    expires_unix: u64,
    keys: BTreeMap<String, String>,
    root_key_ids: Vec<String>,
    root_threshold: usize,
    release_key_ids: Vec<String>,
    release_threshold: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseMetadata {
    schema_version: u16,
    role: String,
    version: u64,
    expires_unix: u64,
    channel: UpdateChannel,
    release_notes: String,
    targets: BTreeMap<String, ReleaseTarget>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseTarget {
    version: String,
    url: String,
    length: u64,
    sha256: String,
}

struct AcceptedRoot {
    version: u64,
    expires_unix: u64,
    root_threshold: usize,
    root_keys: BTreeMap<String, VerifyingKey>,
    release_threshold: usize,
    release_keys: BTreeMap<String, VerifyingKey>,
}

/// Verifies root and release envelopes and selects one authenticated target.
/// Exact payload bytes are signature-checked before either payload is parsed.
///
/// Root version `N+1` must be signed by both the trusted root `N` threshold and
/// its own new threshold. Skips and rollback are rejected. A same-version root
/// may introduce release keys but must preserve the embedded root role exactly.
///
/// # Errors
///
/// Returns a sanitized trust, expiry, rollback, channel, platform, or target error.
pub fn verify_update_metadata(
    trusted: &TrustedRoot,
    root_envelope: &[u8],
    release_envelope: &[u8],
    policy: &UpdateVerificationPolicy<'_>,
) -> Result<VerifiedUpdate, UpdateVerificationError> {
    verify_update_metadata_chain(trusted, &[root_envelope], release_envelope, policy)
}

/// Verifies a bounded sequential root chain followed by release metadata.
/// Every root transition is exact `N+1` and dual-threshold authorized. A
/// missing intermediate, repeated transition, or chain longer than 16 fails.
///
/// # Errors
///
/// Returns a sanitized trust, expiry, rollback, channel, platform, or target error.
pub fn verify_update_metadata_chain(
    trusted: &TrustedRoot,
    root_envelopes: &[&[u8]],
    release_envelope: &[u8],
    policy: &UpdateVerificationPolicy<'_>,
) -> Result<VerifiedUpdate, UpdateVerificationError> {
    if policy.now_unix < policy.high_water.trusted_unix_time {
        return Err(UpdateVerificationError::ClockRollback);
    }
    if root_envelopes.len() > MAX_ROOT_CHAIN {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    let mut current = trusted.clone();
    for (index, envelope) in root_envelopes.iter().enumerate() {
        let accepted = accept_root(&current, envelope, policy.now_unix, true)?;
        if index > 0 && accepted.version != current.version.saturating_add(1) {
            return Err(UpdateVerificationError::InvalidRootRotation);
        }
        current = trusted_from_accepted(accepted);
    }
    if current.release_keys.is_empty()
        || current.release_threshold == 0
        || current.expires_unix < policy.now_unix
    {
        return Err(if current.expires_unix < policy.now_unix {
            UpdateVerificationError::Expired
        } else {
            UpdateVerificationError::InvalidRootRotation
        });
    }
    if current.version < policy.high_water.root_version {
        return Err(UpdateVerificationError::MetadataRollback);
    }
    let release_payload = verify_envelope(
        release_envelope,
        "release",
        &current.release_keys,
        current.release_threshold,
    )?;
    let release: ReleaseMetadata = serde_json::from_slice(&release_payload)
        .map_err(|_| UpdateVerificationError::MalformedMetadata)?;
    if release.schema_version != 1 || release.role != "release" || release.version == 0 {
        return Err(UpdateVerificationError::MalformedMetadata);
    }
    if release.expires_unix < policy.now_unix {
        return Err(UpdateVerificationError::Expired);
    }
    if release.version < policy.high_water.metadata_version {
        return Err(UpdateVerificationError::MetadataRollback);
    }
    if policy.high_water.metadata_version != 0
        && release.version > policy.high_water.metadata_version.saturating_add(1)
    {
        return Err(UpdateVerificationError::MetadataFastForward);
    }
    if release.channel != policy.channel {
        return Err(UpdateVerificationError::ChannelMismatch);
    }
    if release.targets.is_empty() || release.targets.len() > MAX_TARGETS {
        return Err(UpdateVerificationError::InvalidTarget);
    }
    if release.release_notes.len() > MAX_RELEASE_NOTES_BYTES
        || release
            .release_notes
            .chars()
            .any(|value| value.is_control() && !matches!(value, '\n' | '\r' | '\t'))
    {
        return Err(UpdateVerificationError::InvalidTarget);
    }
    let target = release
        .targets
        .get(policy.platform)
        .ok_or(UpdateVerificationError::PlatformUnavailable)?;
    let version =
        Version::parse(&target.version).map_err(|_| UpdateVerificationError::InvalidTarget)?;
    let current_version = Version::parse(policy.current_version)
        .map_err(|_| UpdateVerificationError::InvalidTarget)?;
    if policy.channel == UpdateChannel::Stable && !version.pre.is_empty() {
        return Err(UpdateVerificationError::StablePrerelease);
    }
    if version < current_version && !policy.allow_downgrade {
        return Err(UpdateVerificationError::DowngradeRejected);
    }
    validate_target(target)?;
    Ok(VerifiedUpdate {
        root_version: current.version,
        metadata_version: release.version,
        version,
        platform: policy.platform.to_owned(),
        artifact_url: Url::parse(&target.url)
            .map_err(|_| UpdateVerificationError::InvalidTarget)?,
        artifact_length: target.length,
        artifact_sha256: target.sha256.clone(),
        release_notes: release.release_notes,
        trusted_unix_time: policy.now_unix,
    })
}

/// Restores the last durably authenticated root from exact signed envelopes.
/// Historical expiry is intentionally not re-applied: expiry was checked when
/// each root was first accepted, and the restored root's own expiry is enforced
/// before it signs releases or successors.
///
/// # Errors
///
/// Rejects an oversized, non-sequential, or incorrectly signed persisted chain.
pub fn restore_trusted_root_chain(
    trusted: &TrustedRoot,
    root_envelopes: &[&[u8]],
) -> Result<TrustedRoot, UpdateVerificationError> {
    if root_envelopes.len() > MAX_ROOT_CHAIN {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    let mut current = trusted.clone();
    for (index, envelope) in root_envelopes.iter().enumerate() {
        let accepted = accept_root(&current, envelope, 0, false)?;
        if index > 0 && accepted.version != current.version.saturating_add(1) {
            return Err(UpdateVerificationError::InvalidRootRotation);
        }
        current = trusted_from_accepted(accepted);
    }
    Ok(current)
}

fn trusted_from_accepted(accepted: AcceptedRoot) -> TrustedRoot {
    TrustedRoot {
        version: accepted.version,
        threshold: accepted.root_threshold,
        keys: accepted.root_keys,
        expires_unix: accepted.expires_unix,
        release_threshold: accepted.release_threshold,
        release_keys: accepted.release_keys,
    }
}

fn accept_root(
    trusted: &TrustedRoot,
    envelope_bytes: &[u8],
    now_unix: u64,
    enforce_expiry: bool,
) -> Result<AcceptedRoot, UpdateVerificationError> {
    let payload = verify_envelope(envelope_bytes, "root", &trusted.keys, trusted.threshold)?;
    let root: RootMetadata =
        serde_json::from_slice(&payload).map_err(|_| UpdateVerificationError::MalformedMetadata)?;
    if root.schema_version != 1
        || root.role != "root"
        || root.version < trusted.version
        || root.version > trusted.version.saturating_add(1)
        || (enforce_expiry && root.expires_unix < now_unix)
    {
        return Err(if enforce_expiry && root.expires_unix < now_unix {
            UpdateVerificationError::Expired
        } else {
            UpdateVerificationError::InvalidRootRotation
        });
    }
    let all_keys = decode_key_map(&root.keys)?;
    let root_keys = select_role_keys(&all_keys, &root.root_key_ids, root.root_threshold)?;
    let release_keys = select_role_keys(&all_keys, &root.release_key_ids, root.release_threshold)?;
    let root_material = root_keys
        .values()
        .map(VerifyingKey::to_bytes)
        .collect::<BTreeSet<_>>();
    if release_keys
        .values()
        .map(VerifyingKey::to_bytes)
        .any(|key| root_material.contains(&key))
    {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    if root.version == trusted.version
        && (root.root_threshold != trusted.threshold || root_keys != trusted.keys)
    {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    verify_envelope(envelope_bytes, "root", &root_keys, root.root_threshold)?;
    Ok(AcceptedRoot {
        version: root.version,
        expires_unix: root.expires_unix,
        root_threshold: root.root_threshold,
        root_keys,
        release_threshold: root.release_threshold,
        release_keys,
    })
}

fn verify_envelope(
    bytes: &[u8],
    role: &str,
    keys: &BTreeMap<String, VerifyingKey>,
    threshold: usize,
) -> Result<Vec<u8>, UpdateVerificationError> {
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(UpdateVerificationError::MetadataTooLarge);
    }
    let envelope: SignedEnvelope =
        serde_json::from_slice(bytes).map_err(|_| UpdateVerificationError::MalformedEnvelope)?;
    if envelope.signatures.is_empty()
        || envelope.signatures.len() > MAX_SIGNATURES
        || envelope.payload.len() > MAX_ENVELOPE_BYTES
    {
        return Err(UpdateVerificationError::MalformedEnvelope);
    }
    let payload = STANDARD
        .decode(envelope.payload.as_bytes())
        .map_err(|_| UpdateVerificationError::MalformedEnvelope)?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(UpdateVerificationError::MetadataTooLarge);
    }
    let mut message = Vec::with_capacity(SIGNATURE_DOMAIN.len() + role.len() + payload.len() + 1);
    message.extend_from_slice(SIGNATURE_DOMAIN);
    message.extend_from_slice(role.as_bytes());
    message.push(0);
    message.extend_from_slice(&payload);
    let mut accepted = BTreeSet::new();
    for candidate in envelope.signatures {
        if accepted.contains(&candidate.key_id) {
            continue;
        }
        let Some(key) = keys.get(&candidate.key_id) else {
            continue;
        };
        let Ok(signature_bytes) = STANDARD.decode(candidate.signature.as_bytes()) else {
            continue;
        };
        let Ok(signature) = Signature::from_slice(&signature_bytes) else {
            continue;
        };
        if key.verify_strict(&message, &signature).is_ok() {
            accepted.insert(candidate.key_id);
        }
    }
    if accepted.len() < threshold {
        return Err(UpdateVerificationError::SignatureThreshold);
    }
    Ok(payload)
}

fn decode_key_map(
    encoded: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, VerifyingKey>, UpdateVerificationError> {
    if encoded.is_empty() || encoded.len() > MAX_KEYS {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    let decoded = encoded
        .iter()
        .map(|(id, value)| {
            if id.is_empty() || id.len() > 128 {
                return Err(UpdateVerificationError::InvalidRootRotation);
            }
            let bytes = decode_public_key(value)?;
            let key = VerifyingKey::from_bytes(&bytes)
                .map_err(|_| UpdateVerificationError::InvalidRootRotation)?;
            if key.is_weak() {
                return Err(UpdateVerificationError::InvalidRootRotation);
            }
            Ok((id.clone(), key))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if decoded
        .values()
        .map(VerifyingKey::to_bytes)
        .collect::<BTreeSet<_>>()
        .len()
        != decoded.len()
    {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    Ok(decoded)
}

fn decode_public_key(value: &str) -> Result<[u8; 32], UpdateVerificationError> {
    let decoded = STANDARD
        .decode(value.as_bytes())
        .map_err(|_| UpdateVerificationError::InvalidRootRotation)?;
    let bytes = decoded
        .try_into()
        .map_err(|_| UpdateVerificationError::InvalidRootRotation)?;
    if STANDARD.encode(bytes) != value {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    Ok(bytes)
}

fn select_role_keys(
    keys: &BTreeMap<String, VerifyingKey>,
    ids: &[String],
    threshold: usize,
) -> Result<BTreeMap<String, VerifyingKey>, UpdateVerificationError> {
    if ids.is_empty() || ids.len() > MAX_KEYS || threshold == 0 || threshold > ids.len() {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    let unique = ids.iter().collect::<BTreeSet<_>>();
    if unique.len() != ids.len() {
        return Err(UpdateVerificationError::InvalidRootRotation);
    }
    ids.iter()
        .map(|id| {
            keys.get(id)
                .copied()
                .map(|key| (id.clone(), key))
                .ok_or(UpdateVerificationError::InvalidRootRotation)
        })
        .collect()
}

fn validate_target(target: &ReleaseTarget) -> Result<(), UpdateVerificationError> {
    let url = Url::parse(&target.url).map_err(|_| UpdateVerificationError::InvalidTarget)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || target.length == 0
        || target.length > MAX_ARTIFACT_BYTES
        || target.sha256.len() != 64
        || !target
            .sha256
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
    {
        return Err(UpdateVerificationError::InvalidTarget);
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    const NOW: u64 = 2_000_000_000;

    fn signing_key(byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[byte; 32])
    }

    fn root_metadata(
        version: u64,
        root_id: &str,
        root_key: &SigningKey,
        release_id: &str,
        release_key: &SigningKey,
    ) -> RootMetadata {
        RootMetadata {
            schema_version: 1,
            role: "root".to_owned(),
            version,
            expires_unix: NOW + 10_000,
            keys: BTreeMap::from([
                (
                    root_id.to_owned(),
                    STANDARD.encode(root_key.verifying_key().to_bytes()),
                ),
                (
                    release_id.to_owned(),
                    STANDARD.encode(release_key.verifying_key().to_bytes()),
                ),
            ]),
            root_key_ids: vec![root_id.to_owned()],
            root_threshold: 1,
            release_key_ids: vec![release_id.to_owned()],
            release_threshold: 1,
        }
    }

    fn envelope<T: Serialize>(role: &str, payload: &T, keys: &[(&str, &SigningKey)]) -> Vec<u8> {
        let payload = serde_json::to_vec(payload).expect("fixture payload");
        let mut message = Vec::new();
        message.extend_from_slice(SIGNATURE_DOMAIN);
        message.extend_from_slice(role.as_bytes());
        message.push(0);
        message.extend_from_slice(&payload);
        serde_json::to_vec(&SignedEnvelope {
            payload: STANDARD.encode(payload),
            signatures: keys
                .iter()
                .map(|(id, key)| MetadataSignature {
                    key_id: (*id).to_owned(),
                    signature: STANDARD.encode(key.sign(&message).to_bytes()),
                })
                .collect(),
        })
        .expect("fixture envelope")
    }

    fn fixtures(
        target_version: &str,
        channel: UpdateChannel,
    ) -> (TrustedRoot, Vec<u8>, Vec<u8>, Vec<u8>) {
        let root_key = signing_key(7);
        let release_key = signing_key(9);
        let trusted = TrustedRoot::from_keys(
            1,
            1,
            [("root-1".to_owned(), root_key.verifying_key().to_bytes())],
        )
        .expect("trusted root");
        let root = RootMetadata {
            schema_version: 1,
            role: "root".to_owned(),
            version: 1,
            expires_unix: NOW + 10_000,
            keys: BTreeMap::from([
                (
                    "root-1".to_owned(),
                    STANDARD.encode(root_key.verifying_key().to_bytes()),
                ),
                (
                    "release-1".to_owned(),
                    STANDARD.encode(release_key.verifying_key().to_bytes()),
                ),
            ]),
            root_key_ids: vec!["root-1".to_owned()],
            root_threshold: 1,
            release_key_ids: vec!["release-1".to_owned()],
            release_threshold: 1,
        };
        let artifact = b"authenticated archive bytes".to_vec();
        let release = ReleaseMetadata {
            schema_version: 1,
            role: "release".to_owned(),
            version: 4,
            expires_unix: NOW + 1_000,
            channel,
            release_notes: "Signed notes".to_owned(),
            targets: BTreeMap::from([(
                "aarch64-apple-darwin".to_owned(),
                ReleaseTarget {
                    version: target_version.to_owned(),
                    url: "https://releases.example.invalid/rottweiler.tar.gz".to_owned(),
                    length: artifact.len() as u64,
                    sha256: sha256_hex(&artifact),
                },
            )]),
        };
        (
            trusted,
            envelope("root", &root, &[("root-1", &root_key)]),
            envelope("release", &release, &[("release-1", &release_key)]),
            artifact,
        )
    }

    fn policy(channel: UpdateChannel) -> UpdateVerificationPolicy<'static> {
        UpdateVerificationPolicy {
            channel,
            platform: "aarch64-apple-darwin",
            current_version: "1.0.0",
            now_unix: NOW,
            high_water: UpdateHighWaterMark::default(),
            allow_downgrade: false,
        }
    }

    #[test]
    fn signatures_cover_exact_bytes_before_metadata_parsing() {
        let (trusted, root, mut release, _) = fixtures("1.1.0", UpdateChannel::Stable);
        let last = release.len() - 2;
        release[last] ^= 1;
        assert!(matches!(
            verify_update_metadata(&trusted, &root, &release, &policy(UpdateChannel::Stable)),
            Err(UpdateVerificationError::MalformedEnvelope
                | UpdateVerificationError::SignatureThreshold)
        ));
    }

    #[test]
    fn unsigned_and_tampered_artifacts_are_rejected() {
        let (trusted, root, release, artifact) = fixtures("1.1.0", UpdateChannel::Stable);
        let verified =
            verify_update_metadata(&trusted, &root, &release, &policy(UpdateChannel::Stable))
                .expect("signed fixture");
        verified.verify_artifact(&artifact).expect("exact artifact");
        let mut tampered = artifact;
        tampered[0] ^= 1;
        assert_eq!(
            verified.verify_artifact(&tampered),
            Err(UpdateVerificationError::ArtifactMismatch)
        );
    }

    #[test]
    fn downgrade_flag_never_weakens_metadata_rollback_protection() {
        let (trusted, root, release, _) = fixtures("0.9.0", UpdateChannel::Stable);
        assert_eq!(
            verify_update_metadata(&trusted, &root, &release, &policy(UpdateChannel::Stable)),
            Err(UpdateVerificationError::DowngradeRejected)
        );
        let mut allowed = policy(UpdateChannel::Stable);
        allowed.allow_downgrade = true;
        allowed.high_water.metadata_version = 5;
        assert_eq!(
            verify_update_metadata(&trusted, &root, &release, &allowed),
            Err(UpdateVerificationError::MetadataRollback)
        );
    }

    #[test]
    fn established_metadata_high_water_rejects_fast_forward() {
        let (trusted, root, release, _) = fixtures("1.1.0", UpdateChannel::Stable);
        let mut fast_forward = policy(UpdateChannel::Stable);
        fast_forward.high_water.metadata_version = 2;
        assert_eq!(
            verify_update_metadata(&trusted, &root, &release, &fast_forward),
            Err(UpdateVerificationError::MetadataFastForward)
        );
        fast_forward.high_water.metadata_version = 3;
        assert!(verify_update_metadata(&trusted, &root, &release, &fast_forward).is_ok());
    }

    #[test]
    fn channel_platform_expiry_and_clock_are_bound() {
        let (trusted, root, release, _) = fixtures("1.1.0", UpdateChannel::Beta);
        assert_eq!(
            verify_update_metadata(&trusted, &root, &release, &policy(UpdateChannel::Stable)),
            Err(UpdateVerificationError::ChannelMismatch)
        );
        let mut wrong_platform = policy(UpdateChannel::Beta);
        wrong_platform.platform = "x86_64-unknown-linux-gnu";
        assert_eq!(
            verify_update_metadata(&trusted, &root, &release, &wrong_platform),
            Err(UpdateVerificationError::PlatformUnavailable)
        );
        let mut clock_rollback = policy(UpdateChannel::Beta);
        clock_rollback.high_water.trusted_unix_time = NOW + 1;
        assert_eq!(
            verify_update_metadata(&trusted, &root, &release, &clock_rollback),
            Err(UpdateVerificationError::ClockRollback)
        );
    }

    #[test]
    fn root_rotation_requires_old_and_new_thresholds_and_exact_next_version() {
        let old = signing_key(1);
        let new = signing_key(2);
        let release = signing_key(3);
        let trusted =
            TrustedRoot::from_keys(1, 1, [("old".to_owned(), old.verifying_key().to_bytes())])
                .expect("old root");
        let metadata = RootMetadata {
            schema_version: 1,
            role: "root".to_owned(),
            version: 2,
            expires_unix: NOW + 1_000,
            keys: BTreeMap::from([
                (
                    "new".to_owned(),
                    STANDARD.encode(new.verifying_key().to_bytes()),
                ),
                (
                    "release".to_owned(),
                    STANDARD.encode(release.verifying_key().to_bytes()),
                ),
            ]),
            root_key_ids: vec!["new".to_owned()],
            root_threshold: 1,
            release_key_ids: vec!["release".to_owned()],
            release_threshold: 1,
        };
        let missing_new = envelope("root", &metadata, &[("old", &old)]);
        assert_eq!(
            accept_root(&trusted, &missing_new, NOW, true).map(|_| ()),
            Err(UpdateVerificationError::SignatureThreshold)
        );
        let dual = envelope("root", &metadata, &[("old", &old), ("new", &new)]);
        assert!(accept_root(&trusted, &dual, NOW, true).is_ok());
        let mut skipped = metadata;
        skipped.version = 3;
        let skipped = envelope("root", &skipped, &[("old", &old), ("new", &new)]);
        assert_eq!(
            accept_root(&trusted, &skipped, NOW, true).map(|_| ()),
            Err(UpdateVerificationError::InvalidRootRotation)
        );
    }

    #[test]
    fn duplicate_key_material_and_cross_role_key_reuse_are_rejected() {
        let root_key = signing_key(41);
        assert_eq!(
            TrustedRoot::from_keys(
                1,
                2,
                [
                    ("root-a".to_owned(), root_key.verifying_key().to_bytes()),
                    ("root-b".to_owned(), root_key.verifying_key().to_bytes()),
                ],
            )
            .map(|_| ()),
            Err(UpdateVerificationError::InvalidRootRotation)
        );

        let trusted = TrustedRoot::from_keys(
            1,
            1,
            [("root".to_owned(), root_key.verifying_key().to_bytes())],
        )
        .expect("trusted root");
        let reused = RootMetadata {
            schema_version: 1,
            role: "root".to_owned(),
            version: 1,
            expires_unix: NOW + 1_000,
            keys: BTreeMap::from([(
                "shared".to_owned(),
                STANDARD.encode(root_key.verifying_key().to_bytes()),
            )]),
            root_key_ids: vec!["shared".to_owned()],
            root_threshold: 1,
            release_key_ids: vec!["shared".to_owned()],
            release_threshold: 1,
        };
        let envelope = envelope(
            "root",
            &reused,
            &[("root", &root_key), ("shared", &root_key)],
        );
        assert_eq!(
            accept_root(&trusted, &envelope, NOW, true).map(|_| ()),
            Err(UpdateVerificationError::InvalidRootRotation)
        );
    }

    #[test]
    fn sequential_root_chain_advances_v1_to_v3_and_rejects_missing_or_rollback() {
        let first = signing_key(21);
        let second = signing_key(22);
        let third = signing_key(23);
        let release_key = signing_key(24);
        let fourth = signing_key(25);
        let fourth_release = signing_key(26);
        let second_release = signing_key(27);
        let trusted = TrustedRoot::from_keys(
            1,
            1,
            [("root-1".to_owned(), first.verifying_key().to_bytes())],
        )
        .expect("v1 root");
        let mut v2_payload = root_metadata(2, "root-2", &second, "release-2", &second_release);
        v2_payload.expires_unix = NOW + 1;
        let v2 = envelope(
            "root",
            &v2_payload,
            &[("root-1", &first), ("root-2", &second)],
        );
        let v3_payload = root_metadata(3, "root-3", &third, "release-3", &release_key);
        let v3 = envelope(
            "root",
            &v3_payload,
            &[("root-2", &second), ("root-3", &third)],
        );
        let artifact = b"v3 artifact";
        let release_payload = ReleaseMetadata {
            schema_version: 1,
            role: "release".to_owned(),
            version: 9,
            expires_unix: NOW + 1_000,
            channel: UpdateChannel::Stable,
            release_notes: String::new(),
            targets: BTreeMap::from([(
                "aarch64-apple-darwin".to_owned(),
                ReleaseTarget {
                    version: "1.3.0".to_owned(),
                    url: "https://releases.example.invalid/v3.tar.gz".to_owned(),
                    length: artifact.len() as u64,
                    sha256: sha256_hex(artifact),
                },
            )]),
        };
        let release = envelope("release", &release_payload, &[("release-3", &release_key)]);
        assert!(
            verify_update_metadata_chain(
                &trusted,
                &[&v2, &v3],
                &release,
                &policy(UpdateChannel::Stable),
            )
            .is_ok()
        );
        assert!(matches!(
            verify_update_metadata_chain(
                &trusted,
                &[&v3],
                &release,
                &policy(UpdateChannel::Stable),
            ),
            Err(UpdateVerificationError::SignatureThreshold
                | UpdateVerificationError::InvalidRootRotation)
        ));
        let mut rollback_policy = policy(UpdateChannel::Stable);
        rollback_policy.high_water.root_version = 4;
        assert_eq!(
            verify_update_metadata_chain(&trusted, &[&v2, &v3], &release, &rollback_policy,),
            Err(UpdateVerificationError::MetadataRollback)
        );

        let persisted = restore_trusted_root_chain(&trusted, &[&v2, &v3])
            .expect("persisted v3 trust restores after historical acceptance");
        let v4_payload = root_metadata(4, "root-4", &fourth, "release-4", &fourth_release);
        let v4 = envelope(
            "root",
            &v4_payload,
            &[("root-3", &third), ("root-4", &fourth)],
        );
        let release_v4 = ReleaseMetadata {
            version: 10,
            expires_unix: NOW + 5_000,
            targets: release_payload.targets,
            ..release_payload
        };
        let release_v4 = envelope("release", &release_v4, &[("release-4", &fourth_release)]);
        let mut late_policy = policy(UpdateChannel::Stable);
        late_policy.now_unix = NOW + 2;
        late_policy.high_water.root_version = 3;
        late_policy.high_water.metadata_version = 9;
        assert!(
            verify_update_metadata_chain(&persisted, &[&v4], &release_v4, &late_policy,).is_ok()
        );
    }
}
