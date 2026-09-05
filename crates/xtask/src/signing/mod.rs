use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rw_types::config::UpdateChannel;
use rw_types::update_contract::{
    ReleaseMetadata as ReleasePayload, RootChainDocument, RootChainEntry,
    RootMetadata as RootPayload, UPDATE_RELEASE_ROLE, UPDATE_ROOT_ROLE,
};
use semver::Version;
use serde::Deserialize;

use super::XtaskError;
mod artifacts;
mod trust;
use artifacts::{
    fill_release, inspect_artifacts, sha256sums, signed_envelope_bytes, validate_release_base_url,
    write_update_bundle,
};
use trust::{
    load_prior_release, load_root_chain, load_signers, parse_key_argument, read_spec,
    validate_release_signers, validate_root,
};

pub(super) fn run(arguments: impl Iterator<Item = String>) -> Result<(), XtaskError> {
    sign_update(&SignUpdateCommand::parse(arguments)?)
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
    let stable_bytes = signed_envelope_bytes(UPDATE_RELEASE_ROLE, &stable, &release_keys)?;
    let beta_bytes = signed_envelope_bytes(UPDATE_RELEASE_ROLE, &beta, &release_keys)?;
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
    let root_bytes = signed_envelope_bytes(UPDATE_ROOT_ROLE, &root, &root_keys)?;
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
