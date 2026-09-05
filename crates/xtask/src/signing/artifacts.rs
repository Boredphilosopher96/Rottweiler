use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer as _, SigningKey};
use rw_types::config::UpdateChannel;
use rw_types::update_contract::{
    MAX_UPDATE_ARTIFACT_BYTES, MAX_UPDATE_RELEASE_NOTES_BYTES, MAX_UPDATE_SELECTOR_BYTES,
    MAX_UPDATE_TARGETS, MetadataSignature, ReleaseMetadata as ReleasePayload, ReleaseTarget,
    SignedEnvelope, UPDATE_RELEASE_ROLE, UPDATE_SCHEMA_VERSION, signature_message,
};
use semver::Version;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use url::Url;

use super::{ArtifactIdentity, ReleasePayloadSpec, XtaskError};

pub(super) fn inspect_artifacts(
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

pub(super) fn digest_file(path: &Path) -> Result<(u64, String), XtaskError> {
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

pub(super) fn fill_release(
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
    if spec.schema_version != UPDATE_SCHEMA_VERSION
        || spec.role != UPDATE_RELEASE_ROLE
        || spec.version == 0
        || spec.expires_unix == 0
        || spec.channel != expected_channel
        || spec.targets.is_empty()
        || spec.targets.len() > MAX_UPDATE_TARGETS
        || spec.release_notes.len() > MAX_UPDATE_RELEASE_NOTES_BYTES
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

pub(super) fn validate_release_base_url(value: &str) -> Result<Url, XtaskError> {
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

pub(super) fn signed_envelope_bytes<T: Serialize>(
    role: &str,
    payload: &T,
    signers: &BTreeMap<String, SigningKey>,
) -> Result<Vec<u8>, XtaskError> {
    let payload = serde_json::to_vec(payload)?;
    let message = signature_message(role, &payload);
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

pub(super) fn sha256sums(
    artifacts: &BTreeMap<String, ArtifactIdentity>,
) -> Result<String, XtaskError> {
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

pub(super) fn write_update_bundle<'a, const N: usize>(
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

pub(super) fn safe_selector(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_UPDATE_SELECTOR_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(super) fn safe_file_name(value: &str) -> bool {
    safe_selector(value) && value != "." && value != ".."
}

pub(super) fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        },
    )
}
