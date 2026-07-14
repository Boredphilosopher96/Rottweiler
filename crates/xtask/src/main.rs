use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use rw_types::config::{ThinkingLevel, UpdateChannel};
use rw_types::{
    AccountingAttribution, Answer, ApprovalBinding, ApprovalDecision, Attachment, AttachmentData,
    Block, BudgetLevel, BudgetScope, BudgetUnit, CacheBreakpoint, ClientCommand, ClientId,
    ClientRole, CommandAckMeta, CommandDescriptor, CommandMeta, CommandOutcome, CommandSource,
    CompactionReason, ContextItemId, ContextItemKind, ContextItemSnapshot, ContextItemState,
    ContextSnapshot, Cost, CostSnapshot, DiffArtifact, EngineError, EngineErrorCategory,
    EngineEvent, EventMeta, ImageRef, McpApprovalReview, McpServerDescriptor, McpServerState,
    ModeId, ModelAlias, ModelAliasDescriptor, ModelCacheBehavior, ModelCapabilities,
    ModelCatalogSnapshot, ModelDescriptor, PermissionAction, PermissionApprovalDescriptor,
    PermissionApprovalScope, PermissionRuleDescriptor, PermissionStateDescriptor, PlanArtifact,
    PlanDecision, PlanStep, PromptDump, PromptTool, ProviderAuthAttemptId, ProviderAuthChallenge,
    ProviderAuthKind, ProviderDescriptor, ProviderNextAction, Question, QuestionId, QuestionOption,
    QuestionResponseKind, RequestId, ReviewFileDecision, ReviewFileStatus, RewindTarget, Role,
    RuntimeServiceDescriptor, RuntimeServiceKind, SequenceId, SessionDescriptor, SessionId,
    SessionReview, SessionReviewFile, ShellId, StoredAttachment, SubagentActivity,
    SubagentDescriptor, SubagentId, SubagentIsolation, SubagentReplayItem, SubagentResult,
    SubagentStatus, ToolCallId, ToolCapability, ToolOutput, ToolOutputPart, ToolOutputStream,
    TouchedFile, TouchedFileStatus, Turn, TurnAccounting, TurnId, TurnMeta, TurnStatus,
    UnifiedDiff, UnrestorablePath, Usage, UserSettingDescriptor, WorkspaceDiff, WorkspaceFileMatch,
    WorkspaceFilePreview, WorkspaceRootDescriptor, WorkspaceStatus,
};
use schemars::{JsonSchema, schema_for};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use ts_rs::{Config as TypeScriptConfig, TS};
use url::Url;

const UPDATE_SIGNATURE_DOMAIN: &[u8] = b"rottweiler-update-metadata-v1\0";
const MAX_UPDATE_SPEC_BYTES: u64 = 768 * 1024;
const MAX_UPDATE_KEYS: usize = 32;
const MAX_UPDATE_TARGETS: usize = 32;
const MAX_UPDATE_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RELEASE_NOTES_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
enum XtaskError {
    #[error(
        "usage:\n  cargo xtask codegen [--check]\n  cargo xtask sign-update release --root-chain PATH --stable-spec PATH --beta-spec PATH --base-url HTTPS_URL/ --now-unix SECONDS [--previous-stable PATH] [--previous-beta PATH] --artifact PATH --platform PLATFORM [--artifact PATH --platform PLATFORM ...] --release-key KEY_ID=PATH [--release-key KEY_ID=PATH ...] --output DIRECTORY\n  cargo xtask sign-update rotate-root --root-spec PATH [--root-chain PATH] --root-key KEY_ID=PATH [--root-key KEY_ID=PATH ...] --output DIRECTORY\n\nEd25519 private-key files are exact 32-byte seeds and must be owned by the current user, mode 0600, regular, and single-link. Root keys are accepted only by the explicit offline rotate-root command."
    )]
    Usage,
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not serialize generated protocol artifact: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("generated artifact is stale: {0}")]
    Stale(PathBuf),
    #[error("invalid sign-update argument: {0}")]
    SignArgument(String),
    #[error("invalid update metadata spec {path}: {reason}")]
    UpdateSpec { path: PathBuf, reason: String },
    #[error("unsafe Ed25519 private-key file {path}: {reason}")]
    PrivateKey { path: PathBuf, reason: String },
    #[error("could not inspect {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Serialize)]
struct ContractFixture {
    turns: Vec<Turn>,
    client_commands: Vec<ClientCommand>,
    engine_events: Vec<EngineEvent>,
}

fn main() -> Result<(), XtaskError> {
    let mut arguments = env::args().skip(1);
    let Some(command) = arguments.next() else {
        return Err(XtaskError::Usage);
    };
    if command == "sign-update" {
        let command = SignUpdateCommand::parse(arguments)?;
        return sign_update(&command);
    }
    if command != "codegen" {
        return Err(XtaskError::Usage);
    }
    let check = match arguments.next().as_deref() {
        None => false,
        Some("--check") => true,
        Some(_) => return Err(XtaskError::Usage),
    };
    if arguments.next().is_some() {
        return Err(XtaskError::Usage);
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let protocol = root.join("protocol");
    let artifacts = generated_artifacts()?;
    for (relative_path, contents) in artifacts {
        let path = protocol.join(relative_path);
        if check {
            check_artifact(&path, &contents)?;
        } else {
            write_artifact(&path, &contents)?;
        }
    }
    Ok(())
}

enum SignUpdateCommand {
    Release(ReleaseSignArgs),
    RotateRoot(RootRotationArgs),
}

struct ReleaseSignArgs {
    root_chain: PathBuf,
    stable_spec: PathBuf,
    beta_spec: PathBuf,
    base_url: String,
    now_unix: u64,
    previous_stable: Option<PathBuf>,
    previous_beta: Option<PathBuf>,
    artifacts: Vec<PathBuf>,
    platforms: Vec<String>,
    release_keys: Vec<(String, PathBuf)>,
    output: PathBuf,
}

struct RootRotationArgs {
    root_spec: PathBuf,
    root_chain: Option<PathBuf>,
    root_keys: Vec<(String, PathBuf)>,
    output: PathBuf,
}

impl SignUpdateCommand {
    fn parse(mut arguments: impl Iterator<Item = String>) -> Result<Self, XtaskError> {
        match arguments.next().as_deref() {
            Some("release") => Self::parse_release(arguments).map(Self::Release),
            Some("rotate-root") => Self::parse_rotation(arguments).map(Self::RotateRoot),
            _ => Err(XtaskError::Usage),
        }
    }

    fn parse_release(
        arguments: impl Iterator<Item = String>,
    ) -> Result<ReleaseSignArgs, XtaskError> {
        let mut root_chain = None;
        let mut stable_spec = None;
        let mut beta_spec = None;
        let mut base_url = None;
        let mut now_unix = None;
        let mut previous_stable = None;
        let mut previous_beta = None;
        let mut artifacts = Vec::new();
        let mut platforms = Vec::new();
        let mut release_keys = Vec::new();
        let mut output = None;
        let mut arguments = arguments;
        while let Some(flag) = arguments.next() {
            let value = arguments.next().ok_or(XtaskError::Usage)?;
            match flag.as_str() {
                "--root-chain" if root_chain.is_none() => {
                    root_chain = Some(PathBuf::from(value));
                }
                "--stable-spec" if stable_spec.is_none() => {
                    stable_spec = Some(PathBuf::from(value));
                }
                "--beta-spec" if beta_spec.is_none() => beta_spec = Some(PathBuf::from(value)),
                "--base-url" if base_url.is_none() => base_url = Some(value),
                "--now-unix" if now_unix.is_none() => {
                    now_unix = Some(value.parse::<u64>().map_err(|_| XtaskError::Usage)?);
                }
                "--previous-stable" if previous_stable.is_none() => {
                    previous_stable = Some(PathBuf::from(value));
                }
                "--previous-beta" if previous_beta.is_none() => {
                    previous_beta = Some(PathBuf::from(value));
                }
                "--artifact" => artifacts.push(PathBuf::from(value)),
                "--platform" => platforms.push(value),
                "--release-key" => release_keys.push(parse_key_argument(&value)?),
                "--output" if output.is_none() => output = Some(PathBuf::from(value)),
                _ => return Err(XtaskError::Usage),
            }
        }
        if artifacts.is_empty() || artifacts.len() != platforms.len() || release_keys.is_empty() {
            return Err(XtaskError::Usage);
        }
        Ok(ReleaseSignArgs {
            root_chain: root_chain.ok_or(XtaskError::Usage)?,
            stable_spec: stable_spec.ok_or(XtaskError::Usage)?,
            beta_spec: beta_spec.ok_or(XtaskError::Usage)?,
            base_url: base_url.ok_or(XtaskError::Usage)?,
            now_unix: now_unix.ok_or(XtaskError::Usage)?,
            previous_stable,
            previous_beta,
            artifacts,
            platforms,
            release_keys,
            output: output.ok_or(XtaskError::Usage)?,
        })
    }

    fn parse_rotation(
        arguments: impl Iterator<Item = String>,
    ) -> Result<RootRotationArgs, XtaskError> {
        let mut root_spec = None;
        let mut root_chain = None;
        let mut root_keys = Vec::new();
        let mut output = None;
        let mut arguments = arguments;
        while let Some(flag) = arguments.next() {
            let value = arguments.next().ok_or(XtaskError::Usage)?;
            match flag.as_str() {
                "--root-spec" if root_spec.is_none() => root_spec = Some(PathBuf::from(value)),
                "--root-chain" if root_chain.is_none() => {
                    root_chain = Some(PathBuf::from(value));
                }
                "--root-key" => root_keys.push(parse_key_argument(&value)?),
                "--output" if output.is_none() => output = Some(PathBuf::from(value)),
                _ => return Err(XtaskError::Usage),
            }
        }
        if root_keys.is_empty() {
            return Err(XtaskError::Usage);
        }
        Ok(RootRotationArgs {
            root_spec: root_spec.ok_or(XtaskError::Usage)?,
            root_chain,
            root_keys,
            output: output.ok_or(XtaskError::Usage)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RootPayload {
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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleasePayloadSpec {
    schema_version: u16,
    role: String,
    version: u64,
    expires_unix: u64,
    channel: UpdateChannel,
    release_notes: String,
    targets: BTreeMap<String, ReleaseTargetSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseTargetSpec {
    version: String,
    url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleasePayload {
    schema_version: u16,
    role: String,
    version: u64,
    expires_unix: u64,
    channel: UpdateChannel,
    release_notes: String,
    targets: BTreeMap<String, ReleaseTarget>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseTarget {
    version: String,
    url: String,
    length: u64,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SignedEnvelope {
    payload: String,
    signatures: Vec<MetadataSignature>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RootChainDocument {
    roots: Vec<RootChainEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RootChainEntry {
    version: u64,
    envelope: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MetadataSignature {
    key_id: String,
    signature: String,
}

struct ArtifactIdentity {
    file_name: String,
    platform: String,
    version: Version,
    length: u64,
    sha256: String,
}

fn sign_update(command: &SignUpdateCommand) -> Result<(), XtaskError> {
    match command {
        SignUpdateCommand::Release(arguments) => sign_release(arguments),
        SignUpdateCommand::RotateRoot(arguments) => rotate_root(arguments),
    }
}

fn sign_release(arguments: &ReleaseSignArgs) -> Result<(), XtaskError> {
    if arguments.now_unix == 0 {
        return Err(XtaskError::SignArgument(
            "fixed release signing time must be positive Unix seconds".to_owned(),
        ));
    }
    let (root_chain, root) = load_root_chain(Some(&arguments.root_chain))?;
    let root = root.ok_or_else(|| XtaskError::UpdateSpec {
        path: arguments.root_chain.clone(),
        reason: "release signing requires a non-empty pre-signed root chain".to_owned(),
    })?;
    if root.expires_unix <= arguments.now_unix {
        return Err(XtaskError::UpdateSpec {
            path: arguments.root_chain.clone(),
            reason: "active root is expired at the fixed release signing time".to_owned(),
        });
    }
    let release_keys = load_signers(&arguments.release_keys)?;
    validate_release_signers(&root, &release_keys, &arguments.root_chain)?;
    let stable: ReleasePayloadSpec = read_spec(&arguments.stable_spec)?;
    let beta: ReleasePayloadSpec = read_spec(&arguments.beta_spec)?;
    validate_new_release_expiry(&stable, arguments.now_unix, &arguments.stable_spec)?;
    validate_new_release_expiry(&beta, arguments.now_unix, &arguments.beta_spec)?;
    let base_url = validate_release_base_url(&arguments.base_url)?;
    let previous_stable = arguments
        .previous_stable
        .as_deref()
        .map(|path| load_prior_release(path, UpdateChannel::Stable, &root, &base_url))
        .transpose()?;
    let previous_beta = arguments
        .previous_beta
        .as_deref()
        .map(|path| load_prior_release(path, UpdateChannel::Beta, &root, &base_url))
        .transpose()?;
    validate_release_epoch(
        stable.version,
        beta.version,
        previous_stable.as_ref(),
        previous_beta.as_ref(),
    )?;
    let artifacts = inspect_artifacts(&arguments.platforms, &arguments.artifacts)?;
    let mut used_artifacts = BTreeSet::new();
    let stable = fill_release(
        stable,
        UpdateChannel::Stable,
        &artifacts,
        &base_url,
        previous_stable.as_ref(),
        &mut used_artifacts,
        &arguments.stable_spec,
    )?;
    let beta = fill_release(
        beta,
        UpdateChannel::Beta,
        &artifacts,
        &base_url,
        previous_beta.as_ref(),
        &mut used_artifacts,
        &arguments.beta_spec,
    )?;
    if used_artifacts.len() != artifacts.len() {
        return Err(XtaskError::SignArgument(
            "every supplied artifact must be used by at least one channel target".to_owned(),
        ));
    }
    let stable_bytes = signed_envelope_bytes("release", &stable, &release_keys)?;
    let beta_bytes = signed_envelope_bytes("release", &beta, &release_keys)?;
    let root_bytes = STANDARD
        .decode(
            root_chain
                .last()
                .ok_or_else(|| XtaskError::UpdateSpec {
                    path: arguments.root_chain.clone(),
                    reason: "release signing requires a non-empty root chain".to_owned(),
                })?
                .envelope
                .as_bytes(),
        )
        .map_err(|_| XtaskError::UpdateSpec {
            path: arguments.root_chain.clone(),
            reason: "latest root envelope is malformed".to_owned(),
        })?;
    let mut root_chain_bytes = serde_json::to_vec(&RootChainDocument { roots: root_chain })?;
    root_chain_bytes.push(b'\n');
    let checksums = sha256sums(&artifacts)?;
    write_update_bundle(
        &arguments.output,
        [
            ("SHA256SUMS", checksums.as_bytes()),
            ("beta.json", beta_bytes.as_slice()),
            ("root-chain.json", root_chain_bytes.as_slice()),
            ("root.json", root_bytes.as_slice()),
            ("stable.json", stable_bytes.as_slice()),
        ],
    )
}

fn validate_release_epoch(
    stable_version: u64,
    beta_version: u64,
    previous_stable: Option<&ReleasePayload>,
    previous_beta: Option<&ReleasePayload>,
) -> Result<(), XtaskError> {
    if stable_version != beta_version {
        return Err(XtaskError::SignArgument(
            "stable and beta metadata must use one shared repository version".to_owned(),
        ));
    }
    match (previous_stable.as_ref(), previous_beta.as_ref()) {
        (None, None) if stable_version != 1 => {
            return Err(XtaskError::SignArgument(
                "the first channel publication must use metadata version 1".to_owned(),
            ));
        }
        (None, None) => {}
        (Some(stable_prior), Some(beta_prior)) => {
            if stable_prior.version != beta_prior.version {
                return Err(XtaskError::SignArgument(
                    "prior stable and beta metadata must use one shared repository version"
                        .to_owned(),
                ));
            }
            let expected = stable_prior.version.checked_add(1).ok_or_else(|| {
                XtaskError::SignArgument(
                    "prior channel metadata version cannot be advanced".to_owned(),
                )
            })?;
            if stable_version != expected {
                return Err(XtaskError::SignArgument(format!(
                    "new channel metadata version must advance exactly from {} to {expected}",
                    stable_prior.version
                )));
            }
        }
        _ => {
            return Err(XtaskError::SignArgument(
                "channel metadata updates require both prior signed channel envelopes".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_new_release_expiry(
    spec: &ReleasePayloadSpec,
    now_unix: u64,
    path: &Path,
) -> Result<(), XtaskError> {
    if spec.expires_unix <= now_unix {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "new release metadata is expired at the fixed release signing time".to_owned(),
        });
    }
    Ok(())
}

fn rotate_root(arguments: &RootRotationArgs) -> Result<(), XtaskError> {
    let root: RootPayload = read_spec(&arguments.root_spec)?;
    let (mut root_chain, previous_root) = load_root_chain(arguments.root_chain.as_deref())?;
    if root_chain.len() >= 16 {
        return Err(XtaskError::UpdateSpec {
            path: arguments
                .root_chain
                .clone()
                .unwrap_or_else(|| arguments.root_spec.clone()),
            reason: "root chain already reached its 16-envelope limit".to_owned(),
        });
    }
    let root_keys = load_signers(&arguments.root_keys)?;
    validate_root(
        &root,
        previous_root.as_ref(),
        &root_keys,
        &arguments.root_spec,
    )?;
    let root_bytes = signed_envelope_bytes("root", &root, &root_keys)?;
    root_chain.push(RootChainEntry {
        version: root.version,
        envelope: STANDARD.encode(&root_bytes),
    });
    let mut root_chain_bytes = serde_json::to_vec(&RootChainDocument { roots: root_chain })?;
    root_chain_bytes.push(b'\n');
    write_update_bundle(
        &arguments.output,
        [
            ("root-chain.json", root_chain_bytes.as_slice()),
            ("root.json", root_bytes.as_slice()),
        ],
    )
}

fn parse_key_argument(value: &str) -> Result<(String, PathBuf), XtaskError> {
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

fn validate_key_id(id: &str) -> Result<(), XtaskError> {
    if id.is_empty()
        || id.len() > 128
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

fn read_spec<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, XtaskError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| XtaskError::Read {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_UPDATE_SPEC_BYTES
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

fn load_root_chain(
    path: Option<&Path>,
) -> Result<(Vec<RootChainEntry>, Option<RootPayload>), XtaskError> {
    let Some(path) = path else {
        return Ok((Vec::new(), None));
    };
    let document: RootChainDocument = read_spec(path)?;
    if document.roots.is_empty() || document.roots.len() > 16 {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "existing root chain must contain between 1 and 16 envelopes".to_owned(),
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
                "root",
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
            "root",
            &root.keys,
            &root.root_key_ids,
            root.root_threshold,
            path,
        )?;
        previous = Some(root);
    }
    Ok((document.roots, previous))
}

fn decode_envelope_payload(envelope: &SignedEnvelope, path: &Path) -> Result<Vec<u8>, XtaskError> {
    let payload =
        STANDARD
            .decode(envelope.payload.as_bytes())
            .map_err(|_| XtaskError::UpdateSpec {
                path: path.to_owned(),
                reason: "metadata payload is not base64".to_owned(),
            })?;
    if payload.len() as u64 > MAX_UPDATE_SPEC_BYTES || STANDARD.encode(&payload) != envelope.payload
    {
        return Err(XtaskError::UpdateSpec {
            path: path.to_owned(),
            reason: "metadata payload is oversized or not canonical base64".to_owned(),
        });
    }
    Ok(payload)
}

fn verify_envelope_role(
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
    let mut message = UPDATE_SIGNATURE_DOMAIN.to_vec();
    message.extend_from_slice(role.as_bytes());
    message.push(0);
    message.extend_from_slice(payload);
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

fn load_signers(entries: &[(String, PathBuf)]) -> Result<BTreeMap<String, SigningKey>, XtaskError> {
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

fn read_private_key(path: &Path) -> Result<SigningKey, XtaskError> {
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

fn validate_root(
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
        "root",
    )?;
    let release_ids = validate_role_ids(
        &root.release_key_ids,
        root.release_threshold,
        &root.keys,
        path,
        "release",
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

fn validate_root_shape(root: &RootPayload, path: &Path) -> Result<(), XtaskError> {
    if root.schema_version != 1
        || root.role != "root"
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
        "root",
    )?;
    let release_ids = validate_role_ids(
        &root.release_key_ids,
        root.release_threshold,
        &root.keys,
        path,
        "release",
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

fn decode_public_keys(
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

fn matching_signer_count(
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

fn signer_matches(signer: Option<&SigningKey>, encoded: Option<&String>) -> bool {
    signer.zip(encoded).is_some_and(|(signer, encoded)| {
        STANDARD.encode(signer.verifying_key().to_bytes()) == *encoded
    })
}

fn validate_role_ids(
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

fn validate_signers_against_root(
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

fn validate_release_signers(
    root: &RootPayload,
    signers: &BTreeMap<String, SigningKey>,
    path: &Path,
) -> Result<(), XtaskError> {
    validate_root_shape(root, path)?;
    validate_signers_against_root(signers, &root.keys, path, "release")?;
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

fn load_prior_release(
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
        "release",
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

fn validate_release_payload(
    payload: &ReleasePayload,
    expected_channel: UpdateChannel,
    base_url: &Url,
    path: &Path,
) -> Result<(), XtaskError> {
    if payload.schema_version != 1
        || payload.role != "release"
        || payload.version == 0
        || payload.expires_unix == 0
        || payload.channel != expected_channel
        || payload.targets.is_empty()
        || payload.targets.len() > MAX_UPDATE_TARGETS
        || payload.release_notes.len() > MAX_RELEASE_NOTES_BYTES
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

fn validate_release_target(
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

fn inspect_artifacts(
    platforms: &[String],
    paths: &[PathBuf],
) -> Result<BTreeMap<String, ArtifactIdentity>, XtaskError> {
    let mut artifacts = BTreeMap::new();
    for (platform, path) in platforms.iter().zip(paths) {
        if !safe_selector(platform) {
            return Err(XtaskError::SignArgument(format!(
                "invalid artifact platform {platform:?}"
            )));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| XtaskError::Read {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() == 0
            || metadata.len() > MAX_UPDATE_ARTIFACT_BYTES
        {
            return Err(XtaskError::SignArgument(format!(
                "artifact {} is not a bounded regular file",
                path.display()
            )));
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| safe_file_name(name))
            .ok_or_else(|| {
                XtaskError::SignArgument(format!(
                    "artifact {} has an unsafe file name",
                    path.display()
                ))
            })?
            .to_owned();
        let version_text = file_name
            .strip_prefix("rottweiler-")
            .and_then(|value| value.strip_suffix(&format!("-{platform}.tar.gz")))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                XtaskError::SignArgument(format!(
                    "artifact {file_name:?} does not encode platform {platform:?}"
                ))
            })?;
        let version = Version::parse(version_text).map_err(|_| {
            XtaskError::SignArgument(format!(
                "artifact {file_name:?} does not encode a semantic version"
            ))
        })?;
        if format!("rottweiler-{version}-{platform}.tar.gz") != file_name
            || artifacts.contains_key(&file_name)
        {
            return Err(XtaskError::SignArgument(format!(
                "duplicate artifact file name {file_name:?}"
            )));
        }
        let (length, sha256) = digest_file(path)?;
        artifacts.insert(
            file_name.clone(),
            ArtifactIdentity {
                file_name,
                platform: platform.clone(),
                version,
                length,
                sha256,
            },
        );
    }
    Ok(artifacts)
}

fn digest_file(path: &Path) -> Result<(u64, String), XtaskError> {
    #[cfg(unix)]
    let mut file = File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| XtaskError::Read {
            path: path.to_owned(),
            source: source.into(),
        })?,
    );
    #[cfg(not(unix))]
    let mut file = File::open(path).map_err(|source| XtaskError::Read {
        path: path.to_owned(),
        source,
    })?;
    let before = file.metadata().map_err(|source| XtaskError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if !before.is_file() || before.len() == 0 || before.len() > MAX_UPDATE_ARTIFACT_BYTES {
        return Err(XtaskError::SignArgument(format!(
            "artifact {} is not a bounded regular file",
            path.display()
        )));
    }
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| XtaskError::Read {
            path: path.to_owned(),
            source,
        })?;
        if read == 0 {
            break;
        }
        length = length
            .checked_add(read as u64)
            .filter(|value| *value <= MAX_UPDATE_ARTIFACT_BYTES)
            .ok_or_else(|| {
                XtaskError::SignArgument("artifact exceeds the update size limit".to_owned())
            })?;
        digest.update(&buffer[..read]);
    }
    let after = file.metadata().map_err(|source| XtaskError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt as _;
        before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
    };
    #[cfg(not(unix))]
    let changed = before.modified().ok() != after.modified().ok();
    if before.len() != length || before.len() != after.len() || changed {
        return Err(XtaskError::SignArgument(format!(
            "artifact {} changed while it was hashed",
            path.display()
        )));
    }
    Ok((length, hex_digest(digest.finalize().as_slice())))
}

fn fill_release(
    spec: ReleasePayloadSpec,
    expected_channel: UpdateChannel,
    artifacts: &BTreeMap<String, ArtifactIdentity>,
    base_url: &Url,
    prior: Option<&ReleasePayload>,
    used_artifacts: &mut BTreeSet<String>,
    path: &Path,
) -> Result<ReleasePayload, XtaskError> {
    let invalid = |reason: &str| XtaskError::UpdateSpec {
        path: path.to_owned(),
        reason: reason.to_owned(),
    };
    if spec.schema_version != 1
        || spec.role != "release"
        || spec.version == 0
        || spec.expires_unix == 0
        || spec.channel != expected_channel
        || spec.targets.is_empty()
        || spec.targets.len() > MAX_UPDATE_TARGETS
        || spec.release_notes.len() > MAX_RELEASE_NOTES_BYTES
        || spec
            .release_notes
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(invalid(
            "release header, channel, notes, or target count is invalid",
        ));
    }
    if prior.is_some_and(|prior| {
        prior
            .targets
            .keys()
            .any(|key| !spec.targets.contains_key(key))
    }) {
        return Err(invalid(
            "release metadata must not drop a prior platform target",
        ));
    }
    let targets = spec
        .targets
        .into_iter()
        .map(|(platform, target)| {
            if !safe_selector(&platform) {
                return Err(invalid("release target platform is invalid"));
            }
            let version = Version::parse(&target.version)
                .map_err(|_| invalid("release target version is not semantic versioning"))?;
            if expected_channel == UpdateChannel::Stable && !version.pre.is_empty() {
                return Err(invalid("stable target must not be a prerelease"));
            }
            let prior_target = prior.and_then(|prior| prior.targets.get(&platform));
            if let Some(prior_target) = prior_target {
                let prior_version = Version::parse(&prior_target.version)
                    .map_err(|_| invalid("prior target version is invalid"))?;
                if version < prior_version {
                    return Err(invalid("release target semantic version must not decrease"));
                }
                if version == prior_version {
                    if target.version != prior_target.version || target.url != prior_target.url {
                        return Err(invalid(
                            "an unchanged target version must carry forward the exact prior target",
                        ));
                    }
                    return Ok((platform, prior_target.clone()));
                }
            }
            let file_name = format!("rottweiler-{version}-{platform}.tar.gz");
            let artifact = artifacts.get(&file_name).ok_or_else(|| {
                invalid("a new channel target has no exact matching artifact in the pool")
            })?;
            let expected_url = base_url
                .join(&file_name)
                .map_err(|_| invalid("release target URL is invalid"))?;
            if artifact.file_name != file_name
                || artifact.platform != platform
                || artifact.version != version
                || target.url != expected_url.as_str()
            {
                return Err(invalid(
                    "release target URL, artifact, version, and platform do not match",
                ));
            }
            used_artifacts.insert(file_name);
            Ok((
                platform,
                ReleaseTarget {
                    version: target.version,
                    url: target.url,
                    length: artifact.length,
                    sha256: artifact.sha256.clone(),
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    Ok(ReleasePayload {
        schema_version: spec.schema_version,
        role: spec.role,
        version: spec.version,
        expires_unix: spec.expires_unix,
        channel: spec.channel,
        release_notes: spec.release_notes,
        targets,
    })
}

fn validate_release_base_url(value: &str) -> Result<Url, XtaskError> {
    let url = Url::parse(value)
        .map_err(|_| XtaskError::SignArgument("release base URL is invalid".to_owned()))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
    {
        return Err(XtaskError::SignArgument(
            "release base URL must be credential-free HTTPS ending in /".to_owned(),
        ));
    }
    Ok(url)
}

fn signed_envelope_bytes<T: Serialize>(
    role: &str,
    payload: &T,
    signers: &BTreeMap<String, SigningKey>,
) -> Result<Vec<u8>, XtaskError> {
    let payload = serde_json::to_vec(payload)?;
    let mut message =
        Vec::with_capacity(UPDATE_SIGNATURE_DOMAIN.len() + role.len() + payload.len() + 1);
    message.extend_from_slice(UPDATE_SIGNATURE_DOMAIN);
    message.extend_from_slice(role.as_bytes());
    message.push(0);
    message.extend_from_slice(&payload);
    let signatures = signers
        .iter()
        .map(|(key_id, key)| MetadataSignature {
            key_id: key_id.clone(),
            signature: STANDARD.encode(key.sign(&message).to_bytes()),
        })
        .collect();
    let envelope = SignedEnvelope {
        payload: STANDARD.encode(payload),
        signatures,
    };
    let mut output = serde_json::to_vec(&envelope)?;
    output.push(b'\n');
    Ok(output)
}

fn sha256sums(artifacts: &BTreeMap<String, ArtifactIdentity>) -> Result<String, XtaskError> {
    let mut by_name = artifacts
        .values()
        .map(|artifact| (&artifact.file_name, &artifact.sha256))
        .collect::<Vec<_>>();
    by_name.sort_unstable();
    let mut output = String::new();
    for (file_name, digest) in by_name {
        use std::fmt::Write as _;
        writeln!(output, "{digest}  {file_name}")
            .map_err(|_| XtaskError::SignArgument("could not format SHA256SUMS".to_owned()))?;
    }
    Ok(output)
}

fn write_update_bundle<'a, const N: usize>(
    output: &Path,
    files: [(&'a str, &'a [u8]); N],
) -> Result<(), XtaskError> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    if output.file_name().is_none() || output.as_os_str().is_empty() {
        return Err(XtaskError::SignArgument(
            "output must name a new directory".to_owned(),
        ));
    }
    match fs::symlink_metadata(output) {
        Ok(_) => {
            return Err(XtaskError::SignArgument(format!(
                "output directory {} already exists",
                output.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(XtaskError::Inspect {
                path: output.to_owned(),
                source,
            });
        }
    }
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| XtaskError::Write {
        path: parent.to_owned(),
        source,
    })?;
    let staging = (0_u32..64)
        .find_map(|counter| {
            let path = parent.join(format!(".sign-update-{}-{counter}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => Some(Ok(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .ok_or_else(|| XtaskError::SignArgument("could not allocate staging directory".to_owned()))?
        .map_err(|source| XtaskError::Write {
            path: parent.to_owned(),
            source,
        })?;
    let guard = OutputStaging(staging.clone());
    #[cfg(unix)]
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).map_err(|source| {
        XtaskError::Write {
            path: staging.clone(),
            source,
        }
    })?;
    for (name, contents) in files {
        if !safe_file_name(name) {
            return Err(XtaskError::SignArgument(
                "output file name is unsafe".to_owned(),
            ));
        }
        let path = staging.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o644);
        let mut file = options.open(&path).map_err(|source| XtaskError::Write {
            path: path.clone(),
            source,
        })?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|source| XtaskError::Write {
                path: path.clone(),
                source,
            })?;
    }
    File::open(&staging)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| XtaskError::Write {
            path: staging.clone(),
            source,
        })?;
    #[cfg(unix)]
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).map_err(|source| {
        XtaskError::Write {
            path: staging.clone(),
            source,
        }
    })?;
    fs::rename(&staging, output).map_err(|source| XtaskError::Write {
        path: output.to_owned(),
        source,
    })?;
    std::mem::forget(guard);
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| XtaskError::Write {
            path: parent.to_owned(),
            source,
        })
}

struct OutputStaging(PathBuf);

impl Drop for OutputStaging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn safe_selector(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn safe_file_name(value: &str) -> bool {
    safe_selector(value) && value != "." && value != ".."
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        },
    )
}

fn generated_artifacts() -> Result<Vec<(PathBuf, String)>, XtaskError> {
    let fixture = contract_fixture();
    Ok(vec![
        (PathBuf::from("types.ts"), generate_typescript()),
        (
            PathBuf::from("schema/block.schema.json"),
            generate_schema::<Block>()?,
        ),
        (
            PathBuf::from("schema/tool-output.schema.json"),
            generate_schema::<ToolOutput>()?,
        ),
        (
            PathBuf::from("schema/client-command.schema.json"),
            generate_schema::<ClientCommand>()?,
        ),
        (
            PathBuf::from("schema/engine-event.schema.json"),
            generate_schema::<EngineEvent>()?,
        ),
        (
            PathBuf::from("fixtures/contract.json"),
            serde_json::to_string_pretty(&fixture)? + "\n",
        ),
        (
            PathBuf::from("fixtures/contract.ts"),
            generate_typescript_fixture(&fixture)?,
        ),
    ])
}

fn generate_typescript_fixture(fixture: &ContractFixture) -> Result<String, serde_json::Error> {
    let fixture_json = serde_json::to_string_pretty(fixture)?;
    Ok(format!(
        "// @generated by `cargo xtask codegen`; do not edit by hand.\n\nimport type {{ ClientCommand, EngineEvent, Turn }} from \"../types\";\n\nexport const contractFixture = {fixture_json} satisfies {{\n  turns: Turn[];\n  client_commands: ClientCommand[];\n  engine_events: EngineEvent[];\n}};\n"
    ))
}

#[allow(clippy::too_many_lines)]
fn generate_typescript() -> String {
    let mut output =
        String::from("// @generated by `cargo xtask codegen`; do not edit by hand.\n\n");
    output.push_str(
        "export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };\n\n",
    );
    output.push_str("export const PROTOCOL_VERSION = ");
    output.push_str(&rw_types::PROTOCOL_VERSION.to_string());
    output.push_str(" as const;\n\n");
    let typescript_config = TypeScriptConfig::default();

    macro_rules! declaration {
        ($type:ty) => {{
            output.push_str("export ");
            output.push_str(&<$type as TS>::decl(&typescript_config));
            output.push_str("\n\n");
        }};
    }

    declaration!(ToolCallId);
    declaration!(SessionId);
    declaration!(ClientId);
    declaration!(RequestId);
    declaration!(TurnId);
    declaration!(ShellId);
    declaration!(QuestionId);
    declaration!(SubagentId);
    declaration!(SubagentIsolation);
    declaration!(SubagentActivity);
    declaration!(SubagentDescriptor);
    declaration!(SubagentReplayItem);
    declaration!(SubagentStatus);
    declaration!(TouchedFileStatus);
    declaration!(TouchedFile);
    declaration!(DiffArtifact);
    declaration!(SubagentResult);
    declaration!(ContextItemId);
    declaration!(ModelAlias);
    declaration!(SequenceId);
    declaration!(Role);
    declaration!(ImageRef);
    declaration!(ToolOutputPart);
    declaration!(ToolOutput);
    declaration!(Block);
    declaration!(TurnMeta);
    declaration!(Turn);
    declaration!(CommandMeta);
    declaration!(EventMeta);
    declaration!(CommandAckMeta);
    declaration!(ClientRole);
    declaration!(AttachmentData);
    declaration!(Attachment);
    declaration!(StoredAttachment);
    declaration!(SessionDescriptor);
    declaration!(CommandDescriptor);
    declaration!(CommandSource);
    declaration!(ModelCacheBehavior);
    declaration!(ModelCapabilities);
    declaration!(ModelDescriptor);
    declaration!(ModelAliasDescriptor);
    declaration!(ProviderDescriptor);
    declaration!(ProviderAuthKind);
    declaration!(ProviderAuthAttemptId);
    declaration!(ProviderAuthChallenge);
    declaration!(ProviderNextAction);
    declaration!(ModelCatalogSnapshot);
    declaration!(UserSettingDescriptor);
    declaration!(McpServerState);
    declaration!(McpServerDescriptor);
    declaration!(McpApprovalReview);
    declaration!(RuntimeServiceKind);
    declaration!(RuntimeServiceDescriptor);
    declaration!(WorkspaceFileMatch);
    declaration!(WorkspaceFilePreview);
    declaration!(WorkspaceStatus);
    declaration!(WorkspaceDiff);
    declaration!(WorkspaceRootDescriptor);
    declaration!(UnifiedDiff);
    declaration!(ApprovalBinding);
    declaration!(ApprovalDecision);
    declaration!(ModeId);
    declaration!(PlanStep);
    declaration!(PlanArtifact);
    declaration!(PlanDecision);
    declaration!(RewindTarget);
    declaration!(ReviewFileDecision);
    declaration!(ReviewFileStatus);
    declaration!(SessionReviewFile);
    declaration!(SessionReview);
    declaration!(QuestionResponseKind);
    declaration!(QuestionOption);
    declaration!(Question);
    declaration!(Answer);
    declaration!(ContextItemKind);
    declaration!(ContextItemState);
    declaration!(ContextItemSnapshot);
    declaration!(CacheBreakpoint);
    declaration!(ContextSnapshot);
    declaration!(AccountingAttribution);
    declaration!(TurnAccounting);
    declaration!(CostSnapshot);
    declaration!(PromptTool);
    declaration!(PromptDump);
    declaration!(PermissionAction);
    declaration!(PermissionApprovalScope);
    declaration!(PermissionRuleDescriptor);
    declaration!(PermissionApprovalDescriptor);
    declaration!(PermissionStateDescriptor);
    declaration!(ClientCommand);
    declaration!(ToolCapability);
    declaration!(ToolOutputStream);
    declaration!(TurnStatus);
    declaration!(ThinkingLevel);
    declaration!(CompactionReason);
    declaration!(BudgetUnit);
    declaration!(BudgetLevel);
    declaration!(BudgetScope);
    declaration!(Usage);
    declaration!(Cost);
    declaration!(UnrestorablePath);
    declaration!(EngineErrorCategory);
    declaration!(EngineError);
    declaration!(CommandOutcome);
    declaration!(EngineEvent);
    output
        .trim_end()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn generate_schema<T: JsonSchema>() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&schema_for!(T)).map(|schema| schema + "\n")
}

fn write_artifact(path: &Path, contents: &str) -> Result<(), XtaskError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| XtaskError::Write {
            path: parent.to_owned(),
            source,
        })?;
    }
    fs::write(path, contents).map_err(|source| XtaskError::Write {
        path: path.to_owned(),
        source,
    })
}

fn check_artifact(path: &Path, expected: &str) -> Result<(), XtaskError> {
    let actual = fs::read_to_string(path).map_err(|source| XtaskError::Read {
        path: path.to_owned(),
        source,
    })?;
    if actual == expected {
        Ok(())
    } else {
        Err(XtaskError::Stale(path.to_owned()))
    }
}

#[allow(clippy::too_many_lines)]
fn contract_fixture() -> ContractFixture {
    let command_meta = CommandMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        client_id: ClientId("client-fixture".to_owned()),
        request_id: RequestId("request-fixture".to_owned()),
    };
    let event_meta = |sequence_id| EventMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        session_id: SessionId("session-fixture".to_owned()),
        sequence_id: SequenceId(sequence_id),
        emitted_at: "2026-01-01T00:00:00Z".to_owned(),
        caused_by: None,
    };
    let subagent_result = |id: &str, session: &str, text: &str| SubagentResult {
        subagent_id: SubagentId(id.to_owned()),
        session_id: SessionId(session.to_owned()),
        status: SubagentStatus::Completed,
        final_text: text.to_owned(),
        touched_files: Vec::new(),
        diff_artifact: None,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: Cost::Unavailable {
            reason: "fixture".to_owned(),
        },
        turns: 1,
        duration_millis: 5,
    };
    let mixed_output = ToolOutput::Mixed {
        parts: vec![
            ToolOutputPart::Text {
                text: "build output".to_owned(),
            },
            ToolOutputPart::Structured {
                value: json!({"passed": 3, "failed": 0}),
            },
            ToolOutputPart::Image {
                media_type: "image/png".to_owned(),
                data: ImageRef::InlineBase64 {
                    data: "iVBORw0KGgo=".to_owned(),
                },
            },
        ],
    };
    let turn = Turn {
        role: Role::Assistant,
        blocks: vec![
            Block::Text {
                text: "Working".to_owned(),
            },
            Block::Thinking {
                content: "Check the repository".to_owned(),
                signature: Some("opaque-signature".to_owned()),
            },
            Block::ToolCall {
                id: ToolCallId("tool-1".to_owned()),
                name: "bash".to_owned(),
                args: json!({"command": "cargo test"}),
            },
            Block::ToolResult {
                id: ToolCallId("tool-1".to_owned()),
                output: mixed_output.clone(),
                is_error: false,
            },
            Block::Image {
                media_type: "image/png".to_owned(),
                data: ImageRef::Url {
                    url: "https://example.invalid/image.png".to_owned(),
                },
            },
            Block::Citation {
                uri: "https://example.invalid/source".to_owned(),
                title: Some("Source".to_owned()),
                excerpt: None,
            },
        ],
        meta: TurnMeta {
            created_at: Some("2026-01-01T00:00:00Z".to_owned()),
            model: Some("fast".to_owned()),
            synthetic: false,
            summary: false,
        },
    };
    let plan_artifact = PlanArtifact {
        title: "Protocol plan".to_owned(),
        summary_md: "Exercise the durable plan contract.".to_owned(),
        steps: vec![PlanStep {
            description: "Verify generated clients".to_owned(),
            files_touched: vec!["protocol/types.ts".to_owned()],
            verification: "cargo xtask codegen --check".to_owned(),
        }],
        open_questions: Vec::new(),
    };
    let review = SessionReview {
        session_id: SessionId("session-fixture".to_owned()),
        files: vec![SessionReviewFile {
            path: "src/main.rs".to_owned(),
            unified_diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n"
                .to_owned(),
            status: ReviewFileStatus::Pending,
            truncated: false,
            unrestorable_reason: None,
            original_hash: "original-hash".to_owned(),
            current_hash: "current-hash".to_owned(),
        }],
    };
    let session_descriptor = SessionDescriptor {
        session_id: SessionId("session-fork".to_owned()),
        title: "Session fork".to_owned(),
        workspace_name: "workspace".to_owned(),
        model: ModelAlias("fast".to_owned()),
        driver_client_id: Some(ClientId("client-fixture".to_owned())),
        shell_active: false,
    };

    ContractFixture {
        turns: vec![turn],
        client_commands: vec![
            ClientCommand::CreateSession {
                meta: command_meta.clone(),
                cwd: "workspace".to_owned(),
                model: Some(ModelAlias("fast".to_owned())),
            },
            ClientCommand::SendMessage {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                content: "Build it".to_owned(),
                attachments: Vec::new(),
            },
            ClientCommand::AttachSession {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                last_seen_sequence: Some(SequenceId(4)),
                role: ClientRole::Observer,
            },
            ClientCommand::AttachSession {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
            ClientCommand::SendMessage {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                content: "Inspect these".to_owned(),
                attachments: vec![
                    Attachment {
                        name: "notes.txt".to_owned(),
                        source_path: Some("docs/notes.txt".to_owned()),
                        media_type: "text/plain".to_owned(),
                        data: AttachmentData::Text {
                            content: "in-band text".to_owned(),
                        },
                    },
                    Attachment {
                        name: "screen.png".to_owned(),
                        source_path: None,
                        media_type: "image/png".to_owned(),
                        data: AttachmentData::InlineBase64 {
                            data: "iVBORw0KGgo=".to_owned(),
                        },
                    },
                ],
            },
            ClientCommand::ApproveTool {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                decision: ApprovalDecision::AllowOnce,
                binding: None,
            },
            ClientCommand::ApprovePlan {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                decision: PlanDecision::Approve,
                revisions: None,
            },
            ClientCommand::AnswerQuestion {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                question_id: QuestionId("question-1".to_owned()),
                answers: vec![Answer {
                    question_id: QuestionId("question-1".to_owned()),
                    values: vec!["yes".to_owned()],
                }],
            },
            ClientCommand::Interrupt {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::SwitchMode {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                mode: ModeId("plan".to_owned()),
            },
            ClientCommand::SwitchModel {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                model: ModelAlias("fast".to_owned()),
                provider: None,
            },
            ClientCommand::Compact {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                instructions: None,
            },
            ClientCommand::Fork {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                at_turn: None,
                operation_id: Some("fork-operation-fixture".to_owned()),
            },
            ClientCommand::Rewind {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                target: RewindTarget::Turn {
                    turn_id: TurnId("turn-fixture".to_owned()),
                },
            },
            ClientCommand::Rewind {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                target: RewindTarget::Checkpoint {
                    checkpoint_id: "checkpoint-1".to_owned(),
                },
            },
            ClientCommand::TakeDriver {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::UserShellStarted {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                command: "python".to_owned(),
            },
            ClientCommand::UserShellEnded {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                shell_id: ShellId("shell-fixture".to_owned()),
                status: 0,
                captured_output: None,
            },
            ClientCommand::PinContext {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                item_id: ContextItemId("context-1".to_owned()),
            },
            ClientCommand::EvictContext {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                item_id: ContextItemId("context-2".to_owned()),
            },
            ClientCommand::GetContext {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::GetCost {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::GetSessionReview {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::ReviewFile {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                path: "src/main.rs".to_owned(),
                decision: ReviewFileDecision::Revert,
                current_hash: "current-hash".to_owned(),
            },
            ClientCommand::SearchSessions {
                meta: command_meta.clone(),
                query: "protocol".to_owned(),
                limit: 25,
            },
            ClientCommand::DumpPrompt {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                turn_id: Some(TurnId("turn-fixture".to_owned())),
            },
            ClientCommand::ListRuntimeServices {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
        ],
        engine_events: vec![
            EngineEvent::CommandAcknowledged {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("request-fixture".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: Some(SessionId("session-fixture".to_owned())),
                outcome: CommandOutcome::Accepted,
            },
            EngineEvent::RuntimeServicesListed {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("runtime-services-fixture".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                services: vec![RuntimeServiceDescriptor {
                    kind: RuntimeServiceKind::Lsp,
                    name: "rust-analyzer".to_owned(),
                }],
            },
            EngineEvent::CommandAcknowledged {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("rejected-request".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: None,
                outcome: CommandOutcome::Rejected {
                    error: EngineError {
                        category: EngineErrorCategory::Protocol,
                        code: "invalid_command".to_owned(),
                        message: "command was rejected".to_owned(),
                        retryable: false,
                        details: None,
                    },
                },
            },
            EngineEvent::SessionCreated {
                meta: event_meta(0),
                driver_client_id: ClientId("client-fixture".to_owned()),
            },
            EngineEvent::DriverChanged {
                meta: event_meta(1),
                driver_client_id: ClientId("client-fixture".to_owned()),
            },
            EngineEvent::TurnStarted {
                meta: event_meta(2),
                turn_id: TurnId("turn-fixture".to_owned()),
            },
            EngineEvent::TextDelta {
                meta: event_meta(3),
                turn_id: TurnId("turn-fixture".to_owned()),
                text: "hello".to_owned(),
            },
            EngineEvent::ThinkingDelta {
                meta: event_meta(4),
                turn_id: TurnId("turn-fixture".to_owned()),
                text: "checking".to_owned(),
                signature: None,
            },
            EngineEvent::ToolCallStarted {
                meta: event_meta(5),
                turn_id: TurnId("turn-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                name: "bash".to_owned(),
                args: json!({"command": "cargo test"}),
                call_index: 0,
            },
            EngineEvent::ToolApprovalNeeded {
                meta: event_meta(6),
                turn_id: TurnId("turn-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                name: "bash".to_owned(),
                args: json!({"command": "cargo test"}),
                capabilities: vec![ToolCapability::Execute],
                rationale: "runs a local command".to_owned(),
                diff: None,
            },
            EngineEvent::ToolOutputDelta {
                meta: event_meta(7),
                turn_id: TurnId("turn-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                stream: ToolOutputStream::Stdout,
                chunk: "running tests".to_owned(),
            },
            EngineEvent::ToolCallFinished {
                meta: event_meta(8),
                turn_id: TurnId("turn-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                output: mixed_output,
                is_error: false,
                call_index: 0,
            },
            EngineEvent::QuestionAsked {
                meta: event_meta(9),
                turn_id: TurnId("turn-fixture".to_owned()),
                question_id: QuestionId("question-1".to_owned()),
                questions: vec![Question {
                    id: QuestionId("question-1".to_owned()),
                    prompt: "Continue?".to_owned(),
                    response_kind: QuestionResponseKind::SelectOne,
                    options: vec![QuestionOption {
                        value: "yes".to_owned(),
                        label: "Yes".to_owned(),
                        description: None,
                    }],
                }],
            },
            EngineEvent::QuestionAnswered {
                meta: event_meta(10),
                turn_id: TurnId("turn-fixture".to_owned()),
                question_id: QuestionId("question-1".to_owned()),
                answers: vec![Answer {
                    question_id: QuestionId("question-1".to_owned()),
                    values: vec!["yes".to_owned()],
                }],
            },
            EngineEvent::TurnFinished {
                meta: event_meta(11),
                turn_id: TurnId("turn-fixture".to_owned()),
                status: TurnStatus::Completed,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: 80,
                    cache_write_tokens: 0,
                    reasoning_tokens: 5,
                },
                cost: Cost::Monetary {
                    amount_micros: 125,
                    currency: "USD".to_owned(),
                },
            },
            EngineEvent::CompactionStarted {
                meta: event_meta(12),
                reason: CompactionReason::Automatic,
            },
            EngineEvent::CompactionAttemptStarted {
                session_id: SessionId("session-fixture".to_owned()),
                summary_turn_id: TurnId("summary-turn".to_owned()),
                attempt: 0,
            },
            EngineEvent::CompactionThinkingDelta {
                session_id: SessionId("session-fixture".to_owned()),
                summary_turn_id: TurnId("summary-turn".to_owned()),
                attempt: 0,
                text: "Identifying durable context".to_owned(),
            },
            EngineEvent::CompactionTextDelta {
                session_id: SessionId("session-fixture".to_owned()),
                summary_turn_id: TurnId("summary-turn".to_owned()),
                attempt: 0,
                text: "## Goal\nContinue the task.".to_owned(),
            },
            EngineEvent::CompactionFinished {
                meta: event_meta(13),
                summary_turn_id: TurnId("summary-turn".to_owned()),
                reclaimed_tokens: 25_000,
                usage: None,
                cost: None,
            },
            EngineEvent::CompactionFailed {
                meta: event_meta(14),
                summary_turn_id: TurnId("failed-summary-turn".to_owned()),
            },
            EngineEvent::SubagentSpawned {
                meta: event_meta(15),
                subagent_id: SubagentId("subagent-1".to_owned()),
                child_session_id: SessionId("child-session-1".to_owned()),
                task: "inspect protocol".to_owned(),
            },
            EngineEvent::SubagentFinished {
                meta: event_meta(16),
                subagent_id: SubagentId("subagent-1".to_owned()),
                result: subagent_result("subagent-1", "child-session-1", "done"),
            },
            EngineEvent::SubagentFinished {
                meta: event_meta(17),
                subagent_id: SubagentId("subagent-2".to_owned()),
                result: subagent_result("subagent-2", "child-session-2", "three files"),
            },
            EngineEvent::ToolOutputPruned {
                meta: event_meta(18),
                tool_call_id: ToolCallId("tool-old".to_owned()),
                reclaimed_tokens: 21_000,
            },
            EngineEvent::ModeChanged {
                meta: event_meta(19),
                mode: ModeId("plan".to_owned()),
            },
            EngineEvent::ModelChanged {
                meta: event_meta(20),
                model: ModelAlias("fast".to_owned()),
                provider: None,
                thinking: Some(ThinkingLevel::Off),
            },
            EngineEvent::ContextItemPinned {
                meta: event_meta(21),
                item_id: ContextItemId("context-1".to_owned()),
                effective_after_agent_turn: 3,
            },
            EngineEvent::ContextItemEvicted {
                meta: event_meta(22),
                item_id: ContextItemId("context-2".to_owned()),
                effective_after_agent_turn: 3,
            },
            EngineEvent::UserShellStateChanged {
                meta: event_meta(23),
                shell_id: ShellId("shell-fixture".to_owned()),
                command: Some("python".to_owned()),
                active: false,
                status: None,
                captured_output: None,
            },
            EngineEvent::Error {
                meta: event_meta(24),
                error: EngineError {
                    category: EngineErrorCategory::Protocol,
                    code: "invalid_command".to_owned(),
                    message: "command was rejected".to_owned(),
                    retryable: false,
                    details: Some(json!({"field": "type"})),
                },
            },
            EngineEvent::PlanSubmitted {
                meta: event_meta(25),
                artifact: plan_artifact.clone(),
            },
            EngineEvent::PlanReviewed {
                meta: event_meta(26),
                artifact: plan_artifact,
                decision: PlanDecision::Approve,
                revisions: None,
            },
            EngineEvent::SessionReviewReady {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("review-ready".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                review: review.clone(),
            },
            EngineEvent::SessionReviewUpdated {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("review-updated".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                path: "src/main.rs".to_owned(),
                decision: ReviewFileDecision::Revert,
                review,
            },
            EngineEvent::SessionReplayCompleted {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("replay-complete".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                through_sequence: Some(SequenceId(25)),
            },
            EngineEvent::SessionForked {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("fork-complete".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                parent_session_id: SessionId("session-fixture".to_owned()),
                child: session_descriptor.clone(),
                at_turn: TurnId("turn-fixture".to_owned()),
            },
            EngineEvent::SessionsSearchReady {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("search-complete".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                query: "protocol".to_owned(),
                sessions: vec![session_descriptor],
                truncated: false,
            },
        ],
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use ed25519_dalek::VerifyingKey;
    use tempfile::TempDir;

    use super::*;

    struct SigningFixture {
        root: TempDir,
        rotation: RootRotationArgs,
        root_key: VerifyingKey,
        artifact: Vec<u8>,
    }

    fn write_private_key(path: &Path, seed: [u8; 32]) -> SigningKey {
        fs::write(path, seed).expect("write private key");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("set private key mode");
        SigningKey::from_bytes(&seed)
    }

    fn fixture() -> SigningFixture {
        let root = tempfile::tempdir().expect("temporary directory");
        let root_private = root.path().join("root.key");
        let release_private = root.path().join("release.key");
        let root_signer = write_private_key(&root_private, [7; 32]);
        let release_signer = write_private_key(&release_private, [9; 32]);
        let artifact_name = "rottweiler-1.2.3-darwin-arm64.tar.gz";
        let artifact_path = root.path().join(artifact_name);
        let artifact = b"deterministic signed release archive".to_vec();
        fs::write(&artifact_path, &artifact).expect("write artifact");
        let root_spec = root.path().join("root-spec.json");
        fs::write(
            &root_spec,
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "role": "root",
                "version": 1,
                "expires_unix": 2_000_000_000_u64,
                "keys": {
                    "release-1": STANDARD.encode(release_signer.verifying_key().to_bytes()),
                    "root-1": STANDARD.encode(root_signer.verifying_key().to_bytes()),
                },
                "root_key_ids": ["root-1"],
                "root_threshold": 1,
                "release_key_ids": ["release-1"],
                "release_threshold": 1,
            }))
            .expect("serialize root spec"),
        )
        .expect("write root spec");
        let release_spec = |channel: &str, version: u64| {
            json!({
                "schema_version": 1,
                "role": "release",
                "version": version,
                "expires_unix": 1_900_000_000_u64,
                "channel": channel,
                "release_notes": "Signed release notes",
                "targets": {
                    "darwin-arm64": {
                        "version": "1.2.3",
                        "url": format!("https://releases.example.invalid/{artifact_name}"),
                    }
                }
            })
        };
        let stable_spec = root.path().join("stable-spec.json");
        let beta_spec = root.path().join("beta-spec.json");
        fs::write(
            &stable_spec,
            serde_json::to_vec(&release_spec("stable", 1)).expect("stable spec"),
        )
        .expect("write stable spec");
        fs::write(
            &beta_spec,
            serde_json::to_vec(&release_spec("beta", 1)).expect("beta spec"),
        )
        .expect("write beta spec");
        let rotation = RootRotationArgs {
            root_spec,
            root_chain: None,
            root_keys: vec![("root-1".to_owned(), root_private)],
            output: root.path().join("initial-root"),
        };
        SigningFixture {
            root,
            rotation,
            root_key: root_signer.verifying_key(),
            artifact,
        }
    }

    fn release_arguments(root: &TempDir, chain: &Path, output_name: &str) -> ReleaseSignArgs {
        ReleaseSignArgs {
            root_chain: chain.to_owned(),
            stable_spec: root.path().join("stable-spec.json"),
            beta_spec: root.path().join("beta-spec.json"),
            base_url: "https://releases.example.invalid/".to_owned(),
            now_unix: 1_800_000_000,
            previous_stable: None,
            previous_beta: None,
            artifacts: vec![root.path().join("rottweiler-1.2.3-darwin-arm64.tar.gz")],
            platforms: vec!["darwin-arm64".to_owned()],
            release_keys: vec![("release-1".to_owned(), root.path().join("release.key"))],
            output: root.path().join(output_name),
        }
    }

    fn decode_release(path: &Path) -> ReleasePayload {
        let envelope: SignedEnvelope =
            serde_json::from_slice(&fs::read(path).expect("release envelope"))
                .expect("parse release envelope");
        serde_json::from_slice(
            &STANDARD
                .decode(envelope.payload.as_bytes())
                .expect("release payload"),
        )
        .expect("parse release payload")
    }

    fn set_release_spec_version(path: &Path, version: u64) {
        let mut spec: serde_json::Value =
            serde_json::from_slice(&fs::read(path).expect("release spec"))
                .expect("parse release spec");
        spec["version"] = json!(version);
        fs::write(path, serde_json::to_vec(&spec).expect("release spec bytes"))
            .expect("write release spec");
    }

    fn sign_argument_reason(result: Result<(), XtaskError>) -> String {
        match result {
            Err(XtaskError::SignArgument(reason)) => reason,
            Err(error) => panic!("expected signing-argument error, got {error}"),
            Ok(()) => panic!("expected signing-argument error"),
        }
    }

    #[test]
    fn signing_is_deterministic_and_covers_exact_canonical_payload_bytes() {
        let fixture = fixture();
        rotate_root(&fixture.rotation).expect("initial root signing");
        let chain = fixture.root.path().join("initial-root/root-chain.json");
        let first = release_arguments(&fixture.root, &chain, "first");
        sign_release(&first).expect("first release signing run");
        let first_output = fixture.root.path().join("first");
        let root_bytes = fs::read(first_output.join("root.json")).expect("root envelope");
        let envelope: SignedEnvelope =
            serde_json::from_slice(&root_bytes).expect("decode root envelope");
        let payload = STANDARD
            .decode(envelope.payload.as_bytes())
            .expect("decode root payload");
        let mut message = UPDATE_SIGNATURE_DOMAIN.to_vec();
        message.extend_from_slice(b"root\0");
        message.extend_from_slice(&payload);
        let signature = STANDARD
            .decode(envelope.signatures[0].signature.as_bytes())
            .expect("decode signature");
        fixture
            .root_key
            .verify_strict(
                &message,
                &ed25519_dalek::Signature::from_slice(&signature).expect("signature bytes"),
            )
            .expect("domain-separated signature");

        let stable_bytes = fs::read(first_output.join("stable.json")).expect("stable envelope");
        let stable_envelope: SignedEnvelope =
            serde_json::from_slice(&stable_bytes).expect("decode stable envelope");
        let stable_payload: ReleasePayload = serde_json::from_slice(
            &STANDARD
                .decode(stable_envelope.payload.as_bytes())
                .expect("decode stable payload"),
        )
        .expect("parse stable payload");
        let target = &stable_payload.targets["darwin-arm64"];
        assert_eq!(target.length, fixture.artifact.len() as u64);
        assert_eq!(
            target.sha256,
            hex_digest(&Sha256::digest(&fixture.artifact))
        );
        assert_eq!(
            fs::read_to_string(first_output.join("SHA256SUMS")).expect("checksums"),
            format!("{}  rottweiler-1.2.3-darwin-arm64.tar.gz\n", target.sha256)
        );

        let second_output = fixture.root.path().join("second");
        let second = release_arguments(&fixture.root, &chain, "second");
        sign_release(&second).expect("second release signing run");
        for name in [
            "SHA256SUMS",
            "beta.json",
            "root-chain.json",
            "root.json",
            "stable.json",
        ] {
            assert_eq!(
                fs::read(first_output.join(name)).expect("first output"),
                fs::read(second_output.join(name)).expect("second output")
            );
        }
    }

    #[test]
    fn unsafe_private_key_mode_and_hard_links_fail_before_output() {
        let mode = fixture();
        let key_path = &mode.rotation.root_keys[0].1;
        fs::set_permissions(key_path, fs::Permissions::from_mode(0o644)).expect("weaken key mode");
        assert!(matches!(
            rotate_root(&mode.rotation),
            Err(XtaskError::PrivateKey { .. })
        ));
        assert!(!mode.root.path().join("initial-root").exists());

        let linked = fixture();
        fs::hard_link(
            &linked.rotation.root_keys[0].1,
            linked.root.path().join("root-key-link"),
        )
        .expect("create hard link");
        assert!(matches!(
            rotate_root(&linked.rotation),
            Err(XtaskError::PrivateKey { .. })
        ));
        assert!(!linked.root.path().join("initial-root").exists());
    }

    #[test]
    fn signer_role_mismatch_is_rejected_before_output() {
        let fixture = fixture();
        rotate_root(&fixture.rotation).expect("initial root");
        let chain = fixture.root.path().join("initial-root/root-chain.json");
        let mut release = release_arguments(&fixture.root, &chain, "role-output");
        release.release_keys[0].0 = "root-1".to_owned();
        assert!(matches!(
            sign_release(&release),
            Err(XtaskError::UpdateSpec { .. })
        ));
        assert!(!fixture.root.path().join("role-output").exists());
    }

    #[test]
    fn release_signing_binds_one_shared_version_and_exact_base_url() {
        let missing_slash = fixture();
        rotate_root(&missing_slash.rotation).expect("initial root");
        let chain = missing_slash
            .root
            .path()
            .join("initial-root/root-chain.json");
        let mut arguments = release_arguments(&missing_slash.root, &chain, "missing-slash");
        arguments.base_url = "https://releases.example.invalid/v1".to_owned();
        assert!(matches!(
            sign_release(&arguments),
            Err(XtaskError::SignArgument(_))
        ));

        let divergent = fixture();
        rotate_root(&divergent.rotation).expect("initial root");
        let chain = divergent.root.path().join("initial-root/root-chain.json");
        let beta_path = divergent.root.path().join("beta-spec.json");
        let mut beta: serde_json::Value =
            serde_json::from_slice(&fs::read(&beta_path).expect("beta spec")).expect("beta JSON");
        beta["version"] = json!(5);
        fs::write(&beta_path, serde_json::to_vec(&beta).expect("beta bytes")).expect("write beta");
        let arguments = release_arguments(&divergent.root, &chain, "divergent");
        assert!(matches!(
            sign_release(&arguments),
            Err(XtaskError::SignArgument(_))
        ));

        let wrong_repository = fixture();
        rotate_root(&wrong_repository.rotation).expect("initial root");
        let chain = wrong_repository
            .root
            .path()
            .join("initial-root/root-chain.json");
        let mut arguments = release_arguments(&wrong_repository.root, &chain, "wrong-repository");
        arguments.base_url = "https://releases.example.invalid/v1/".to_owned();
        assert!(matches!(
            sign_release(&arguments),
            Err(XtaskError::UpdateSpec { .. })
        ));
    }

    #[test]
    fn channels_advance_independently_only_from_signed_prior_targets() {
        let fixture = fixture();
        rotate_root(&fixture.rotation).expect("initial root");
        let chain = fixture.root.path().join("initial-root/root-chain.json");
        let initial = release_arguments(&fixture.root, &chain, "initial-release");
        sign_release(&initial).expect("initial release");
        let prior_stable = fixture.root.path().join("initial-release/stable.json");
        let prior_beta = fixture.root.path().join("initial-release/beta.json");

        let beta_name = "rottweiler-1.3.0-beta.1-darwin-arm64.tar.gz";
        let beta_artifact = fixture.root.path().join(beta_name);
        fs::write(&beta_artifact, b"new beta artifact").expect("beta artifact");
        let stable_spec = json!({
            "schema_version": 1,
            "role": "release",
            "version": 2,
            "expires_unix": 1_950_000_000_u64,
            "channel": "stable",
            "release_notes": "Stable remains unchanged",
            "targets": {"darwin-arm64": {
                "version": "1.2.3",
                "url": "https://releases.example.invalid/rottweiler-1.2.3-darwin-arm64.tar.gz"
            }}
        });
        let beta_spec = json!({
            "schema_version": 1,
            "role": "release",
            "version": 2,
            "expires_unix": 1_950_000_000_u64,
            "channel": "beta",
            "release_notes": "New beta",
            "targets": {"darwin-arm64": {
                "version": "1.3.0-beta.1",
                "url": format!("https://releases.example.invalid/{beta_name}")
            }}
        });
        fs::write(
            fixture.root.path().join("stable-spec.json"),
            serde_json::to_vec(&stable_spec).expect("stable spec"),
        )
        .expect("write stable spec");
        fs::write(
            fixture.root.path().join("beta-spec.json"),
            serde_json::to_vec(&beta_spec).expect("beta spec"),
        )
        .expect("write beta spec");
        let mut next = release_arguments(&fixture.root, &chain, "independent");
        next.now_unix = 1_925_000_000;
        next.previous_stable = Some(prior_stable.clone());
        next.previous_beta = Some(prior_beta.clone());
        next.artifacts = vec![beta_artifact];
        sign_release(&next).expect("independent beta release");

        let old = decode_release(&prior_stable);
        let new_stable = decode_release(&fixture.root.path().join("independent/stable.json"));
        let new_beta = decode_release(&fixture.root.path().join("independent/beta.json"));
        assert_eq!(new_stable.version, 2);
        assert_eq!(new_beta.version, 2);
        assert_eq!(
            new_stable.targets["darwin-arm64"],
            old.targets["darwin-arm64"]
        );
        assert_eq!(new_beta.targets["darwin-arm64"].version, "1.3.0-beta.1");

        let mut crossed = release_arguments(&fixture.root, &chain, "crossed");
        crossed.previous_stable = Some(prior_beta);
        crossed.previous_beta = Some(prior_stable);
        crossed.artifacts = vec![fixture.root.path().join(beta_name)];
        assert!(matches!(
            sign_release(&crossed),
            Err(XtaskError::UpdateSpec { .. })
        ));
    }

    #[test]
    fn release_signing_rejects_expired_active_root_and_new_channel_specs() {
        let expired_root = fixture();
        rotate_root(&expired_root.rotation).expect("initial root");
        let chain = expired_root
            .root
            .path()
            .join("initial-root/root-chain.json");
        let mut arguments = release_arguments(&expired_root.root, &chain, "expired-root");
        arguments.now_unix = 0;
        let reason = sign_argument_reason(sign_release(&arguments));
        assert!(reason.contains("positive Unix seconds"));
        arguments.now_unix = 2_000_000_000;
        assert!(matches!(
            sign_release(&arguments),
            Err(XtaskError::UpdateSpec { path, reason })
                if path == chain && reason.contains("active root is expired")
        ));

        let expired_stable = fixture();
        rotate_root(&expired_stable.rotation).expect("initial root");
        let chain = expired_stable
            .root
            .path()
            .join("initial-root/root-chain.json");
        let stable_path = expired_stable.root.path().join("stable-spec.json");
        let mut arguments = release_arguments(&expired_stable.root, &chain, "expired-stable");
        arguments.now_unix = 1_900_000_000;
        assert!(matches!(
            sign_release(&arguments),
            Err(XtaskError::UpdateSpec { path, reason })
                if path == stable_path && reason.contains("expired")
        ));

        let expired_beta = fixture();
        rotate_root(&expired_beta.rotation).expect("initial root");
        let chain = expired_beta
            .root
            .path()
            .join("initial-root/root-chain.json");
        let stable_path = expired_beta.root.path().join("stable-spec.json");
        let beta_path = expired_beta.root.path().join("beta-spec.json");
        let mut stable: serde_json::Value =
            serde_json::from_slice(&fs::read(&stable_path).expect("stable spec"))
                .expect("stable JSON");
        stable["expires_unix"] = json!(1_950_000_000_u64);
        fs::write(
            &stable_path,
            serde_json::to_vec(&stable).expect("stable bytes"),
        )
        .expect("write stable");
        let mut arguments = release_arguments(&expired_beta.root, &chain, "expired-beta");
        arguments.now_unix = 1_900_000_000;
        assert!(matches!(
            sign_release(&arguments),
            Err(XtaskError::UpdateSpec { path, reason })
                if path == beta_path && reason.contains("expired")
        ));
    }

    #[test]
    fn release_metadata_epochs_start_at_one_and_advance_exactly_from_matching_priors() {
        let fixture = fixture();
        rotate_root(&fixture.rotation).expect("initial root");
        let chain = fixture.root.path().join("initial-root/root-chain.json");
        let stable_spec = fixture.root.path().join("stable-spec.json");
        let beta_spec = fixture.root.path().join("beta-spec.json");

        set_release_spec_version(&stable_spec, 2);
        set_release_spec_version(&beta_spec, 2);
        let reason = sign_argument_reason(sign_release(&release_arguments(
            &fixture.root,
            &chain,
            "invalid-initial-epoch",
        )));
        assert!(reason.contains("first channel publication"));

        set_release_spec_version(&stable_spec, 1);
        set_release_spec_version(&beta_spec, 1);
        sign_release(&release_arguments(&fixture.root, &chain, "initial-release"))
            .expect("initial release");
        let prior_stable = fixture.root.path().join("initial-release/stable.json");
        let prior_beta = fixture.root.path().join("initial-release/beta.json");

        set_release_spec_version(&stable_spec, 3);
        set_release_spec_version(&beta_spec, 3);
        let mut skipped = release_arguments(&fixture.root, &chain, "skipped-epoch");
        skipped.previous_stable = Some(prior_stable.clone());
        skipped.previous_beta = Some(prior_beta.clone());
        let reason = sign_argument_reason(sign_release(&skipped));
        assert!(reason.contains("advance exactly"));

        set_release_spec_version(&stable_spec, 2);
        set_release_spec_version(&beta_spec, 2);
        let mut second = release_arguments(&fixture.root, &chain, "second-release");
        second.previous_stable = Some(prior_stable.clone());
        second.previous_beta = Some(prior_beta);
        second.artifacts.clear();
        second.platforms.clear();
        sign_release(&second).expect("second release");

        set_release_spec_version(&stable_spec, 3);
        set_release_spec_version(&beta_spec, 3);
        let mut split = release_arguments(&fixture.root, &chain, "split-prior-epochs");
        split.previous_stable = Some(prior_stable);
        split.previous_beta = Some(fixture.root.path().join("second-release/beta.json"));
        split.artifacts.clear();
        split.platforms.clear();
        let reason = sign_argument_reason(sign_release(&split));
        assert!(reason.contains("prior stable and beta metadata"));
    }

    #[test]
    fn prior_rollback_unsigned_prior_and_unused_artifacts_are_rejected() {
        let fixture = fixture();
        rotate_root(&fixture.rotation).expect("initial root");
        let chain = fixture.root.path().join("initial-root/root-chain.json");
        let initial = release_arguments(&fixture.root, &chain, "initial-release");
        sign_release(&initial).expect("initial release");
        let prior_stable = fixture.root.path().join("initial-release/stable.json");
        let prior_beta = fixture.root.path().join("initial-release/beta.json");

        let unsigned = fixture.root.path().join("unsigned-stable.json");
        let mut envelope: SignedEnvelope =
            serde_json::from_slice(&fs::read(&prior_stable).expect("prior stable"))
                .expect("prior envelope");
        envelope.signatures[0].signature = STANDARD.encode([0_u8; 64]);
        fs::write(
            &unsigned,
            serde_json::to_vec(&envelope).expect("unsigned envelope"),
        )
        .expect("write unsigned prior");
        let mut unsigned_args = release_arguments(&fixture.root, &chain, "unsigned");
        unsigned_args.previous_stable = Some(unsigned);
        unsigned_args.previous_beta = Some(prior_beta.clone());
        assert!(matches!(
            sign_release(&unsigned_args),
            Err(XtaskError::UpdateSpec { .. })
        ));

        let mut stale = release_arguments(&fixture.root, &chain, "stale");
        stale.previous_stable = Some(prior_stable);
        stale.previous_beta = Some(prior_beta);
        assert!(matches!(
            sign_release(&stale),
            Err(XtaskError::SignArgument(_))
        ));

        let downgrade_name = "rottweiler-1.1.0-darwin-arm64.tar.gz";
        let downgrade_path = fixture.root.path().join(downgrade_name);
        fs::write(&downgrade_path, b"older signed artifact").expect("downgrade artifact");
        let channel_spec = |channel: &str, target_version: &str, target_name: &str| {
            json!({
                "schema_version": 1,
                "role": "release",
                "version": 2,
                "expires_unix": 1_950_000_000_u64,
                "channel": channel,
                "release_notes": "rollback fixture",
                "targets": {"darwin-arm64": {
                    "version": target_version,
                    "url": format!("https://releases.example.invalid/{target_name}")
                }}
            })
        };
        fs::write(
            fixture.root.path().join("stable-spec.json"),
            serde_json::to_vec(&channel_spec("stable", "1.1.0", downgrade_name))
                .expect("downgrade stable"),
        )
        .expect("write downgrade stable");
        fs::write(
            fixture.root.path().join("beta-spec.json"),
            serde_json::to_vec(&channel_spec(
                "beta",
                "1.2.3",
                "rottweiler-1.2.3-darwin-arm64.tar.gz",
            ))
            .expect("carry beta"),
        )
        .expect("write carry beta");
        let mut downgrade = release_arguments(&fixture.root, &chain, "downgrade");
        downgrade.previous_stable = Some(fixture.root.path().join("initial-release/stable.json"));
        downgrade.previous_beta = Some(fixture.root.path().join("initial-release/beta.json"));
        downgrade.artifacts = vec![downgrade_path];
        assert!(matches!(
            sign_release(&downgrade),
            Err(XtaskError::UpdateSpec { .. })
        ));

        let current_name = "rottweiler-1.2.3-darwin-arm64.tar.gz";
        fs::write(
            fixture.root.path().join("stable-spec.json"),
            serde_json::to_vec(&channel_spec("stable", "1.2.3", current_name))
                .expect("current stable"),
        )
        .expect("write current stable");
        fs::write(
            fixture.root.path().join("beta-spec.json"),
            serde_json::to_vec(&channel_spec("beta", "1.2.3", current_name)).expect("current beta"),
        )
        .expect("write current beta");
        let unused_name = "rottweiler-2.0.0-darwin-arm64.tar.gz";
        let unused_path = fixture.root.path().join(unused_name);
        fs::write(&unused_path, b"unused artifact").expect("unused artifact");
        let mut unused = release_arguments(&fixture.root, &chain, "unused");
        unused.previous_stable = Some(fixture.root.path().join("initial-release/stable.json"));
        unused.previous_beta = Some(fixture.root.path().join("initial-release/beta.json"));
        unused.artifacts.push(unused_path);
        unused.platforms.push("darwin-arm64".to_owned());
        assert!(matches!(
            sign_release(&unused),
            Err(XtaskError::SignArgument(_))
        ));
    }

    #[test]
    fn release_mode_has_no_root_private_key_argument() {
        assert!(matches!(
            SignUpdateCommand::parse(
                ["release", "--root-key", "root-1=/private/root.key"]
                    .into_iter()
                    .map(str::to_owned)
            ),
            Err(XtaskError::Usage)
        ));
        assert!(matches!(
            SignUpdateCommand::parse(
                [
                    "rotate-root",
                    "--release-key",
                    "release-1=/private/release.key"
                ]
                .into_iter()
                .map(str::to_owned)
            ),
            Err(XtaskError::Usage)
        ));
    }

    #[test]
    fn rotation_appends_only_with_old_and_new_root_thresholds() {
        let fixture = fixture();
        rotate_root(&fixture.rotation).expect("initial signing");
        let root = &fixture.root;
        let prior_chain = root.path().join("initial-root/root-chain.json");
        let new_key_path = root.path().join("new-root.key");
        let new_signer = write_private_key(&new_key_path, [11; 32]);
        let old_public =
            STANDARD.encode(SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes());
        let release_public =
            STANDARD.encode(SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes());
        fs::write(
            root.path().join("root-spec.json"),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "role": "root",
                "version": 2,
                "expires_unix": 2_100_000_000_u64,
                "keys": {
                    "new-root": STANDARD.encode(new_signer.verifying_key().to_bytes()),
                    "release-1": release_public,
                    "root-1": old_public,
                },
                "root_key_ids": ["new-root"],
                "root_threshold": 1,
                "release_key_ids": ["release-1"],
                "release_threshold": 1,
            }))
            .expect("rotation root spec"),
        )
        .expect("write rotation root spec");

        let rotated = RootRotationArgs {
            root_spec: root.path().join("root-spec.json"),
            root_chain: Some(prior_chain.clone()),
            root_keys: vec![
                ("root-1".to_owned(), root.path().join("root.key")),
                ("new-root".to_owned(), new_key_path.clone()),
            ],
            output: root.path().join("rotated"),
        };
        rotate_root(&rotated).expect("dual-threshold rotation");
        let rotated_chain = root.path().join("rotated/root-chain.json");
        let chain: RootChainDocument = read_spec(&rotated_chain).expect("read rotated root chain");
        assert_eq!(chain.roots.len(), 2);
        let (_, accepted) = load_root_chain(Some(&rotated_chain)).expect("verify rotated chain");
        assert_eq!(accepted.expect("last root").version, 2);

        let conflicting_chain = root.path().join("conflicting-root-chain.json");
        let mut conflicting = chain;
        conflicting.roots.push(conflicting.roots[1].clone());
        fs::write(
            &conflicting_chain,
            serde_json::to_vec(&conflicting).expect("conflicting chain"),
        )
        .expect("write conflicting chain");
        assert!(matches!(
            load_root_chain(Some(&conflicting_chain)),
            Err(XtaskError::UpdateSpec { .. })
        ));

        let missing_old = RootRotationArgs {
            root_spec: root.path().join("root-spec.json"),
            root_chain: Some(prior_chain),
            root_keys: vec![("new-root".to_owned(), new_key_path)],
            output: root.path().join("missing-old"),
        };
        assert!(matches!(
            rotate_root(&missing_old),
            Err(XtaskError::UpdateSpec { .. })
        ));
        assert!(!root.path().join("missing-old").exists());
    }
}
