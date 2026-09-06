use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use rw_types::config::UpdateChannel;
use rw_types::update_contract::{
    MAX_UPDATE_ARTIFACT_BYTES, MAX_UPDATE_KEYS, MAX_UPDATE_PAYLOAD_BYTES,
    MAX_UPDATE_RELEASE_NOTES_BYTES, MAX_UPDATE_ROOT_CHAIN_ENTRIES, MAX_UPDATE_SELECTOR_BYTES,
    MAX_UPDATE_SIGNATURES, MAX_UPDATE_TARGETS, ReleaseMetadata as ReleasePayload, ReleaseTarget,
    RootChainDocument, RootChainEntry, RootMetadata as RootPayload, SignedEnvelope,
    UPDATE_RELEASE_ROLE, UPDATE_ROOT_ROLE, UPDATE_SCHEMA_VERSION, signature_message,
};
use semver::Version;
use serde::Deserialize;
use url::Url;

use super::XtaskError;
use super::artifacts::safe_selector;

pub(super) fn parse_key_argument(value: &str) -> Result<(String, PathBuf), XtaskError> {
    let (id, path) = value
        .split_once('=')
        .ok_or_else(|| XtaskError::SignArgument("key arguments must be KEY_ID=PATH".to_owned()))?;
    validate_key_id(id)?;
    if path.is_empty() {
        return Err(XtaskError::SignArgument(
            "private-key path must not be empty".to_owned(),
        ));
    }
    Ok((id.to_owned(), PathBuf::from(path)))
}

pub(super) fn validate_key_id(id: &str) -> Result<(), XtaskError> {
    if id.is_empty()
        || id.len() > MAX_UPDATE_SELECTOR_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(XtaskError::SignArgument(format!(
            "invalid signing key id {id:?}"
        )));
    }
    Ok(())
}

pub(super) fn read_spec<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, XtaskError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| XtaskError::Read {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > u64::try_from(MAX_UPDATE_PAYLOAD_BYTES).unwrap_or(u64::MAX)
    {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "spec must be a regular, non-symlink file within the size limit".to_owned(),
        });
    }
    let bytes = fs::read(path).map_err(|source| XtaskError::Read {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|_| XtaskError::UpdateSpec {
        path: path.to_owned(),
        reason: "spec is not valid strict JSON for its metadata role".to_owned(),
    })
}

pub(super) fn load_root_chain(
    path: Option<&Path>,
) -> Result<(Vec<RootChainEntry>, Option<RootPayload>), XtaskError> {
    let Some(path) = path else {
        return Ok((Vec::new(), None));
    };
    let document: RootChainDocument = read_spec(path)?;
    if document.roots.is_empty() || document.roots.len() > MAX_UPDATE_ROOT_CHAIN_ENTRIES {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: format!(
                "existing root chain must contain between 1 and {MAX_UPDATE_ROOT_CHAIN_ENTRIES} envelopes"
            ),
        });
    }
    let mut previous: Option<RootPayload> = None;
    for entry in &document.roots {
        let envelope_bytes =
            STANDARD
                .decode(entry.envelope.as_bytes())
                .map_err(|_| XtaskError::UpdateSpec {
                    path: path.to_owned(),
                    reason: "root chain envelope is not base64".to_owned(),
                })?;
        if STANDARD.encode(&envelope_bytes) != entry.envelope {
            return Err(XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "root chain envelope is not canonical base64".to_owned(),
            });
        }
        let envelope: SignedEnvelope =
            serde_json::from_slice(&envelope_bytes).map_err(|_| XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "root chain contains a malformed envelope".to_owned(),
            })?;
        let payload_bytes = decode_envelope_payload(&envelope, path)?;
        let root: RootPayload =
            serde_json::from_slice(&payload_bytes).map_err(|_| XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "root chain contains malformed root metadata".to_owned(),
            })?;
        validate_root_shape(&root, path)?;
        if root.version != entry.version {
            return Err(XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "root chain entry version does not match its signed payload".to_owned(),
            });
        }
        if let Some(prior) = previous.as_ref() {
            if root.version != prior.version.saturating_add(1) {
                return Err(XtaskError::UpdateSpec {
                    path: path.to_owned(),
                    reason: "root chain versions must be exact sequential increments".to_owned(),
                });
            }
            verify_envelope_role(
                &envelope,
                &payload_bytes,
                UPDATE_ROOT_ROLE,
                &prior.keys,
                &prior.root_key_ids,
                prior.root_threshold,
                path,
            )?;
        } else if root.version != 1 {
            return Err(XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "root chain must begin at version 1".to_owned(),
            });
        }
        verify_envelope_role(
            &envelope,
            &payload_bytes,
            UPDATE_ROOT_ROLE,
            &root.keys,
            &root.root_key_ids,
            root.root_threshold,
            path,
        )?;
        previous = Some(root);
    }
    Ok((document.roots, previous))
}

pub(super) fn decode_envelope_payload(
    envelope: &SignedEnvelope,
    path: &Path,
) -> Result<Vec<u8>, XtaskError> {
    if envelope.signatures.is_empty() || envelope.signatures.len() > MAX_UPDATE_SIGNATURES {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "metadata signature count is invalid".to_owned(),
        });
    }
    let payload =
        STANDARD
            .decode(envelope.payload.as_bytes())
            .map_err(|_| XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "metadata payload is not base64".to_owned(),
            })?;
    if payload.len() > MAX_UPDATE_PAYLOAD_BYTES || STANDARD.encode(&payload) != envelope.payload {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "metadata payload is oversized or not canonical base64".to_owned(),
        });
    }
    Ok(payload)
}

pub(super) fn verify_envelope_role(
    envelope: &SignedEnvelope,
    payload: &[u8],
    role: &str,
    encoded_keys: &BTreeMap<String, String>,
    role_ids: &[String],
    threshold: usize,
    path: &Path,
) -> Result<(), XtaskError> {
    let keys = decode_public_keys(encoded_keys, path)?;
    let permitted = role_ids.iter().collect::<BTreeSet<_>>();
    let message = signature_message(role, payload);
    let mut accepted = BTreeSet::new();
    for candidate in &envelope.signatures {
        if !permitted.contains(&candidate.key_id) || accepted.contains(&candidate.key_id) {
            continue;
        }
        let Some(key) = keys.get(&candidate.key_id) else {
            continue;
        };
        let Ok(bytes) = STANDARD.decode(candidate.signature.as_bytes()) else {
            continue;
        };
        if STANDARD.encode(&bytes) != candidate.signature {
            continue;
        }
        let Ok(signature) = Signature::from_slice(&bytes) else {
            continue;
        };
        if key.verify_strict(&message, &signature).is_ok() {
            accepted.insert(candidate.key_id.clone());
        }
    }
    if accepted.len() < threshold {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: format!("root chain does not meet the {role} signature threshold"),
        });
    }
    Ok(())
}

pub(super) fn load_signers(
    entries: &[(String, PathBuf)],
) -> Result<BTreeMap<String, SigningKey>, XtaskError> {
    let mut signers = BTreeMap::new();
    let mut unique_material = BTreeSet::new();
    for (id, path) in entries {
        if signers.contains_key(id) {
            return Err(XtaskError::SignArgument(format!(
                "duplicate signing key id {id:?}"
            )));
        }
        let signer = read_private_key(path)?;
        if !unique_material.insert(signer.verifying_key().to_bytes()) {
            return Err(XtaskError::SignArgument(
                "signing keys must use distinct key material".to_owned(),
            ));
        }
        signers.insert(id.clone(), signer);
    }
    Ok(signers)
}

pub(super) fn read_private_key(path: &Path) -> Result<SigningKey, XtaskError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Err(XtaskError::SignArgument(
            "sign-update requires Unix private-key permission checks".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let descriptor = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| XtaskError::PrivateKey {
            path: path.to_owned(),
            reason: format!("could not safely open the file: {source}"),
        })?;
        let mut file = File::from(descriptor);
        let before = file.metadata().map_err(|source| XtaskError::Inspect {
            path: path.to_owned(),
            source,
        })?;
        if !before.is_file()
            || before.nlink() != 1
            || before.mode() & 0o777 != 0o600
            || before.uid() != rustix::process::geteuid().as_raw()
            || before.len() != 32
        {
            return Err(XtaskError::PrivateKey {
                path: path.to_owned(),
                reason:
                    "expected an owned, mode-0600, regular, single-link, exact 32-byte seed file"
                        .to_owned(),
            });
        }
        let mut seed = [0_u8; 32];
        file.read_exact(&mut seed)
            .map_err(|source| XtaskError::Read {
                path: path.to_owned(),
                source,
            })?;
        let mut trailing = [0_u8; 1];
        if file
            .read(&mut trailing)
            .map_err(|source| XtaskError::Read {
                path: path.to_owned(),
                source,
            })?
            != 0
        {
            return Err(XtaskError::PrivateKey {
                path: path.to_owned(),
                reason: "private key changed while it was read".to_owned(),
            });
        }
        let after = file.metadata().map_err(|source| XtaskError::Inspect {
            path: path.to_owned(),
            source,
        })?;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
        {
            return Err(XtaskError::PrivateKey {
                path: path.to_owned(),
                reason: "private key changed while it was read".to_owned(),
            });
        }
        let signing_key = SigningKey::from_bytes(&seed);
        seed.fill(0);
        Ok(signing_key)
    }
}

pub(super) fn validate_root(
    root: &RootPayload,
    previous: Option<&RootPayload>,
    root_signers: &BTreeMap<String, SigningKey>,
    path: &Path,
) -> Result<(), XtaskError> {
    let invalid = |reason: &str| XtaskError::UpdateSpec {
        path: path.to_owned(),
        reason: reason.to_owned(),
    };
    validate_root_shape(root, path)?;
    match previous {
        Some(previous) if root.version != previous.version.saturating_add(1) => {
            return Err(invalid(
                "new root version must exactly follow the existing root chain",
            ));
        }
        None if root.version != 1 => {
            return Err(invalid("a new root chain must begin at version 1"));
        }
        _ => {}
    }
    let root_ids = validate_role_ids(
        &root.root_key_ids,
        root.root_threshold,
        &root.keys,
        path,
        UPDATE_ROOT_ROLE,
    )?;
    let release_ids = validate_role_ids(
        &root.release_key_ids,
        root.release_threshold,
        &root.keys,
        path,
        UPDATE_RELEASE_ROLE,
    )?;
    if !root_ids.is_disjoint(&release_ids) {
        return Err(invalid("root and release roles must use distinct key ids"));
    }
    if matching_signer_count(root_signers, &root.keys, &root_ids) < root.root_threshold {
        return Err(invalid(
            "root signer set does not meet the new root threshold",
        ));
    }
    if let Some(previous) = previous {
        let prior_ids = previous
            .root_key_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if matching_signer_count(root_signers, &previous.keys, &prior_ids) < previous.root_threshold
        {
            return Err(invalid(
                "root signer set does not meet the previous root threshold",
            ));
        }
    }
    if root_signers.keys().any(|id| {
        !signer_matches(root_signers.get(id), root.keys.get(id))
            && previous
                .is_none_or(|prior| !signer_matches(root_signers.get(id), prior.keys.get(id)))
    }) {
        return Err(invalid(
            "root signer id or public key is absent from both old and new roots",
        ));
    }
    Ok(())
}

pub(super) fn validate_root_shape(root: &RootPayload, path: &Path) -> Result<(), XtaskError> {
    if root.schema_version != UPDATE_SCHEMA_VERSION
        || root.role != UPDATE_ROOT_ROLE
        || root.version == 0
        || root.expires_unix == 0
        || root.keys.is_empty()
        || root.keys.len() > MAX_UPDATE_KEYS
    {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "root header or key count is invalid".to_owned(),
        });
    }
    let root_ids = validate_role_ids(
        &root.root_key_ids,
        root.root_threshold,
        &root.keys,
        path,
        UPDATE_ROOT_ROLE,
    )?;
    let release_ids = validate_role_ids(
        &root.release_key_ids,
        root.release_threshold,
        &root.keys,
        path,
        UPDATE_RELEASE_ROLE,
    )?;
    if !root_ids.is_disjoint(&release_ids) {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "root and release roles must use distinct key ids".to_owned(),
        });
    }
    let _ = decode_public_keys(&root.keys, path)?;
    Ok(())
}

pub(super) fn decode_public_keys(
    encoded_keys: &BTreeMap<String, String>,
    path: &Path,
) -> Result<BTreeMap<String, VerifyingKey>, XtaskError> {
    let decoded = encoded_keys
        .iter()
        .map(|(id, encoded)| {
            validate_key_id(id)?;
            let decoded =
                STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|_| XtaskError::UpdateSpec {
                        path: path.to_owned(),
                        reason: "root public key is not canonical base64".to_owned(),
                    })?;
            let bytes: [u8; 32] = decoded.try_into().map_err(|_| XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "root public key is not 32 bytes".to_owned(),
            })?;
            let key = VerifyingKey::from_bytes(&bytes).map_err(|_| XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "root public key is invalid".to_owned(),
            })?;
            if key.is_weak() || STANDARD.encode(bytes) != *encoded {
                return Err(XtaskError::UpdateSpec {
                    path: path.to_owned(),
                    reason: "root public key is weak or not canonical base64".to_owned(),
                });
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
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "root keys must use distinct public-key material".to_owned(),
        });
    }
    Ok(decoded)
}

pub(super) fn matching_signer_count(
    signers: &BTreeMap<String, SigningKey>,
    declared: &BTreeMap<String, String>,
    permitted: &BTreeSet<String>,
) -> usize {
    signers
        .iter()
        .filter(|(id, signer)| {
            permitted.contains(*id)
                && declared.get(*id).is_some_and(|encoded| {
                    STANDARD.encode(signer.verifying_key().to_bytes()) == *encoded
                })
        })
        .count()
}

pub(super) fn signer_matches(signer: Option<&SigningKey>, encoded: Option<&String>) -> bool {
    signer.zip(encoded).is_some_and(|(signer, encoded)| {
        STANDARD.encode(signer.verifying_key().to_bytes()) == *encoded
    })
}

pub(super) fn validate_role_ids(
    ids: &[String],
    threshold: usize,
    keys: &BTreeMap<String, String>,
    path: &Path,
    role: &str,
) -> Result<BTreeSet<String>, XtaskError> {
    let unique = ids.iter().cloned().collect::<BTreeSet<_>>();
    if ids.is_empty()
        || ids.len() > MAX_UPDATE_KEYS
        || unique.len() != ids.len()
        || threshold == 0
        || threshold > ids.len()
        || ids.iter().any(|id| !keys.contains_key(id))
    {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: format!("{role} key ids or threshold are invalid"),
        });
    }
    Ok(unique)
}

pub(super) fn validate_signers_against_root(
    signers: &BTreeMap<String, SigningKey>,
    declared: &BTreeMap<String, String>,
    path: &Path,
    role: &str,
) -> Result<(), XtaskError> {
    for (id, signer) in signers {
        let Some(expected) = declared.get(id) else {
            return Err(XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: format!("{role} signer id {id:?} is absent from root keys"),
            });
        };
        if STANDARD.encode(signer.verifying_key().to_bytes()) != *expected {
            return Err(XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: format!("{role} signer {id:?} does not match its declared public key"),
            });
        }
    }
    Ok(())
}

pub(super) fn validate_release_signers(
    root: &RootPayload,
    signers: &BTreeMap<String, SigningKey>,
    path: &Path,
) -> Result<(), XtaskError> {
    validate_root_shape(root, path)?;
    validate_signers_against_root(signers, &root.keys, path, UPDATE_RELEASE_ROLE)?;
    let release_ids = root
        .release_key_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if signers.keys().any(|id| !release_ids.contains(id))
        || matching_signer_count(signers, &root.keys, &release_ids) < root.release_threshold
    {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason:
                "release signer ids do not belong to the active release role or meet its threshold"
                    .to_owned(),
        });
    }
    Ok(())
}

pub(super) fn load_prior_release(
    path: &Path,
    expected_channel: UpdateChannel,
    root: &RootPayload,
    base_url: &Url,
) -> Result<ReleasePayload, XtaskError> {
    let envelope: SignedEnvelope = read_spec(path)?;
    let payload_bytes = decode_envelope_payload(&envelope, path)?;
    verify_envelope_role(
        &envelope,
        &payload_bytes,
        UPDATE_RELEASE_ROLE,
        &root.keys,
        &root.release_key_ids,
        root.release_threshold,
        path,
    )?;
    let payload: ReleasePayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "prior release payload is malformed".to_owned(),
        })?;
    validate_release_payload(&payload, expected_channel, base_url, path)?;
    Ok(payload)
}

pub(super) fn validate_release_payload(
    payload: &ReleasePayload,
    expected_channel: UpdateChannel,
    base_url: &Url,
    path: &Path,
) -> Result<(), XtaskError> {
    if payload.schema_version != UPDATE_SCHEMA_VERSION
        || payload.role != UPDATE_RELEASE_ROLE
        || payload.version == 0
        || payload.expires_unix == 0
        || payload.channel != expected_channel
        || payload.targets.is_empty()
        || payload.targets.len() > MAX_UPDATE_TARGETS
        || payload.release_notes.len() > MAX_UPDATE_RELEASE_NOTES_BYTES
        || payload
            .release_notes
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "prior release header, channel, notes, or target count is invalid".to_owned(),
        });
    }
    for (platform, target) in &payload.targets {
        validate_release_target(platform, target, expected_channel, base_url, path)?;
    }
    Ok(())
}

pub(super) fn validate_release_target(
    platform: &str,
    target: &ReleaseTarget,
    channel: UpdateChannel,
    base_url: &Url,
    path: &Path,
) -> Result<Version, XtaskError> {
    let invalid = || XtaskError::UpdateSpec {
        path: path.to_owned(),
        reason: "release target URL, version, length, digest, or platform is invalid".to_owned(),
    };
    if !safe_selector(platform) {
        return Err(invalid());
    }
    let version = Version::parse(&target.version).map_err(|_| invalid())?;
    if channel == UpdateChannel::Stable && !version.pre.is_empty() {
        return Err(invalid());
    }
    let file_name = format!("rottweiler-{version}-{platform}.tar.gz");
    let url = Url::parse(&target.url).map_err(|_| invalid())?;
    if base_url.join(&file_name).ok().as_ref() != Some(&url)
        || target.length == 0
        || target.length > MAX_UPDATE_ARTIFACT_BYTES
        || target.sha256.len() != 64
        || !target
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid());
    }
    Ok(version)
}
