//! Signed self-update orchestration and atomic generation activation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Cursor, Read as _, Write as _},
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _, symlink},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::GzDecoder;
use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{
    EMBEDDED_UPDATE_BASE_URL, TrustedRoot, UpdateChannel, UpdateHighWaterMark,
    UpdateVerificationPolicy, VerifiedUpdate, prepare_update_network, restore_trusted_root_chain,
    verify_update_metadata_chain,
};
use serde::{Deserialize, Serialize};
use tar::EntryType;
use url::Url;

const METADATA_LIMIT: usize = 1024 * 1024;
const ARTIFACT_LIMIT: usize = 64 * 1024 * 1024;
const STATE_LIMIT: u64 = 2 * 1024 * 1024;
const ROOT_CHAIN_LIMIT: usize = 16;
const MOUNTS_LIMIT: u64 = 1024 * 1024;
const OS_RELEASE_LIMIT: u64 = 4096;
const ARCHIVE_ENTRY_LIMIT: usize = 6;
const EXPANDED_LIMIT: u64 = 160 * 1024 * 1024;
const STATE_MARKER: &str = ".update-state-initialized";

#[derive(Clone, Copy, Debug)]
pub(crate) struct UpgradeOptions {
    pub(crate) channel: Option<UpdateChannel>,
    pub(crate) allow_downgrade: bool,
    pub(crate) rollback: bool,
    pub(crate) timeout_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    version: String,
    platform: String,
    files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct UpgradeState {
    schema_version: u16,
    highest_root_version: u64,
    highest_metadata_version: u64,
    trusted_unix_time: u64,
    trusted_root_chain: Vec<RootChainEntry>,
    active: Generation,
    previous: Option<Generation>,
    pending_release_notes: Option<PendingReleaseNotes>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingReleaseNotes {
    version: String,
    notes: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RootChainEntry {
    version: u64,
    envelope: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootChainDocument {
    roots: Vec<RootChainEntry>,
}

#[derive(Debug)]
struct InstallLayout {
    prefix: PathBuf,
    versions: PathBuf,
    current_version: String,
}

struct RootSelection {
    trusted: TrustedRoot,
    successor_envelopes: Vec<Vec<u8>>,
    accepted_chain: Vec<RootChainEntry>,
}

struct UpgradeLock {
    path: PathBuf,
    owner: PathBuf,
}

impl UpgradeLock {
    fn acquire(prefix: &Path) -> Result<Self> {
        let path = prefix.join(".install-lock");
        for attempt in 0..2 {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let lock = Self {
                        owner: path.join("pid"),
                        path: path.clone(),
                    };
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                        .into_diagnostic()?;
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&lock.owner)
                        .into_diagnostic()?;
                    writeln!(file, "{}", std::process::id()).into_diagnostic()?;
                    file.sync_all().into_diagnostic()?;
                    sync_directory(&path)?;
                    return Ok(lock);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && attempt == 0
                        && reclaim_stale_upgrade_lock(&path)? => {}
                Err(_) => {
                    return Err(miette!(
                        "another install or `rw upgrade` process is already active"
                    ));
                }
            }
        }
        Err(miette!(
            "another install or `rw upgrade` process is already active"
        ))
    }
}

impl Drop for UpgradeLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.owner);
        let _ = fs::remove_dir(&self.path);
    }
}

fn reclaim_stale_upgrade_lock(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path).into_diagnostic()?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(miette!(
            "install lock is unsafe; inspect it before retrying"
        ));
    }
    let owner = path.join("pid");
    let Ok(bytes) = read_private_file(&owner, 64, "install lock owner") else {
        return Ok(false);
    };
    let raw = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|value| *value > 0)
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| miette!("install lock owner is invalid; inspect it before retrying"))?;
    match rustix::process::test_kill_process(raw) {
        Ok(()) | Err(rustix::io::Errno::PERM) => Ok(false),
        Err(rustix::io::Errno::SRCH) => {
            fs::remove_file(&owner).into_diagnostic()?;
            fs::remove_dir(path).into_diagnostic()?;
            Ok(true)
        }
        Err(error) => Err(std::io::Error::from(error)).into_diagnostic(),
    }
}

struct StagingGuard(PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) async fn run(options: UpgradeOptions) -> Result<()> {
    if options.timeout_ms < 250 || options.timeout_ms > 120_000 {
        return Err(miette!(
            "--timeout-ms must be between 250 and 120000 milliseconds"
        ));
    }
    let executable = std::env::current_exe().into_diagnostic()?;
    let layout = InstallLayout::from_executable(&executable)?;
    refuse_wsl_drvfs(&layout.prefix)?;
    let _lock = UpgradeLock::acquire(&layout.prefix)?;
    recover_pending_state(&layout)?;
    let mut state = load_or_bootstrap_state(&layout)?;
    if options.rollback {
        rollback(&layout, &mut state)?;
        println!("rolled back to Rottweiler {}", state.active.version);
        return Ok(());
    }

    run_signed_upgrade(&layout, state, options).await
}

async fn run_signed_upgrade(
    layout: &InstallLayout,
    mut state: UpgradeState,
    options: UpgradeOptions,
) -> Result<()> {
    let client = prepare_update_network().into_diagnostic()?;
    for warning in client.warnings() {
        eprintln!("warning: {warning}");
    }
    let channel = options.channel.unwrap_or(client.channel());
    let base = embedded_update_base_url()?;
    let root_url = base
        .join("root-chain.json")
        .map_err(|_| miette!("compiled update metadata origin is invalid"))?;
    let release_url = base
        .join(match channel {
            UpdateChannel::Stable => "stable.json",
            UpdateChannel::Beta => "beta.json",
        })
        .map_err(|_| miette!("compiled update metadata origin is invalid"))?;
    let timeout = Duration::from_millis(options.timeout_ms);
    let root_bytes = client
        .fetch(&root_url, METADATA_LIMIT, timeout)
        .await
        .into_diagnostic()?;
    let release_bytes = client
        .fetch(&release_url, METADATA_LIMIT, timeout)
        .await
        .into_diagnostic()?;
    let embedded = TrustedRoot::embedded().into_diagnostic()?;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| miette!("system clock is before the Unix epoch"))?
        .as_secs();
    let policy = UpdateVerificationPolicy {
        channel,
        platform: release_platform(),
        current_version: env!("CARGO_PKG_VERSION"),
        now_unix,
        high_water: UpdateHighWaterMark {
            root_version: state.highest_root_version,
            metadata_version: state.highest_metadata_version,
            trusted_unix_time: state.trusted_unix_time,
        },
        allow_downgrade: options.allow_downgrade,
    };
    let fetched = decode_root_chain(&root_bytes)?;
    let roots = restore_and_select_roots(
        &embedded,
        state.highest_root_version,
        &state.trusted_root_chain,
        fetched,
    )?;
    let root_slices = roots
        .successor_envelopes
        .iter()
        .map(Vec::as_slice)
        .collect::<Vec<_>>();
    let verified =
        verify_update_metadata_chain(&roots.trusted, &root_slices, &release_bytes, &policy)
            .into_diagnostic()?;
    if roots
        .accepted_chain
        .last()
        .is_some_and(|entry| entry.version != verified.root_version())
    {
        return Err(miette!(
            "signed update root-chain version binding is invalid"
        ));
    }
    let accepted_root_chain = roots.accepted_chain;
    if verified.version().to_string() == state.active.version {
        advance_high_water(&mut state, &verified);
        state.trusted_root_chain = accepted_root_chain;
        write_state_atomic(layout, &state)?;
        println!("Rottweiler {} is already installed", state.active.version);
        return Ok(());
    }
    install_verified_update(
        &client,
        layout,
        state,
        verified,
        accepted_root_chain,
        timeout,
    )
    .await
}

async fn install_verified_update(
    client: &rw_core::UpdateNetworkClient,
    layout: &InstallLayout,
    state: UpgradeState,
    verified: VerifiedUpdate,
    accepted_root_chain: Vec<RootChainEntry>,
    timeout: Duration,
) -> Result<()> {
    let artifact_limit = usize::try_from(verified.artifact_length())
        .ok()
        .filter(|value| *value <= ARTIFACT_LIMIT)
        .ok_or_else(|| miette!("signed update artifact exceeds the supported download limit"))?;
    let artifact = client
        .fetch(verified.artifact_url(), artifact_limit, timeout)
        .await
        .into_diagnostic()?;
    verified.verify_artifact(&artifact).into_diagnostic()?;

    let generation = stage_generation(layout, &verified, &artifact).await?;
    let next = UpgradeState {
        schema_version: 1,
        highest_root_version: verified.root_version(),
        highest_metadata_version: verified.metadata_version(),
        trusted_unix_time: verified.trusted_unix_time(),
        trusted_root_chain: accepted_root_chain,
        active: generation,
        previous: Some(state.active),
        pending_release_notes: (!verified.release_notes().is_empty()).then(|| {
            PendingReleaseNotes {
                version: verified.version().to_string(),
                notes: verified.release_notes().to_owned(),
            }
        }),
    };
    commit_activation(layout, &next)?;
    println!(
        "installed Rottweiler {}; restart `rw` to use the new generation",
        next.active.version
    );
    Ok(())
}

pub(crate) fn show_pending_release_notes() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let Ok(layout) = InstallLayout::from_executable(&executable) else {
        return;
    };
    let Ok(_lock) = UpgradeLock::acquire(&layout.prefix) else {
        return;
    };
    let Ok(mut state) = load_state_document(&layout) else {
        return;
    };
    let Some(pending) = state.pending_release_notes.clone() else {
        return;
    };
    if pending.version != env!("CARGO_PKG_VERSION") || pending.version != state.active.version {
        return;
    }
    if validate_layout_state(&layout, &state).is_err() {
        return;
    }
    state.pending_release_notes = None;
    if write_state_atomic(&layout, &state).is_err() {
        return;
    }
    eprintln!(
        "Rottweiler {} release notes:\n{}",
        pending.version, pending.notes
    );
}

impl InstallLayout {
    fn from_executable(executable: &Path) -> Result<Self> {
        let executable = executable.canonicalize().into_diagnostic()?;
        if executable.file_name().and_then(|value| value.to_str()) != Some("rw") {
            return Err(unsupported_layout());
        }
        let bin = executable.parent().ok_or_else(unsupported_layout)?;
        if bin.file_name().and_then(|value| value.to_str()) != Some("bin") {
            return Err(unsupported_layout());
        }
        let generation = bin.parent().ok_or_else(unsupported_layout)?;
        let version = generation
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| safe_selector(value))
            .ok_or_else(unsupported_layout)?;
        let versions = generation.parent().ok_or_else(unsupported_layout)?;
        if versions.file_name().and_then(|value| value.to_str()) != Some("versions") {
            return Err(unsupported_layout());
        }
        let prefix = versions.parent().ok_or_else(unsupported_layout)?.to_owned();
        let expected_current = PathBuf::from("versions").join(version);
        if fs::read_link(prefix.join("current")).ok().as_deref() != Some(&expected_current)
            || fs::read_link(prefix.join("bin/rw")).ok().as_deref()
                != Some(Path::new("../current/bin/rw"))
        {
            return Err(unsupported_layout());
        }
        validate_managed_directory(&prefix, "install prefix")?;
        validate_managed_directory(versions, "versions directory")?;
        Ok(Self {
            prefix,
            versions: versions.to_owned(),
            current_version: version.to_owned(),
        })
    }
}

fn unsupported_layout() -> miette::Report {
    unsupported_layout_for(std::env::var_os("ROTTWEILER_PACKAGE_MANAGER").as_deref())
}

fn unsupported_layout_for(package_manager: Option<&std::ffi::OsStr>) -> miette::Report {
    if package_manager == Some(std::ffi::OsStr::new("homebrew")) {
        return miette!(
            "this Rottweiler installation is managed by Homebrew; run `brew upgrade rottweiler` (self-update never modifies package-managed files)"
        );
    }
    miette!(
        "self-update requires the official versioned installation layout; reinstall with the signed release install.sh (package-managed and direct-copy binaries are not modified)"
    )
}

fn decode_root_chain(bytes: &[u8]) -> Result<Vec<RootChainEntry>> {
    let document: RootChainDocument = serde_json::from_slice(bytes)
        .map_err(|_| miette!("signed update root-chain document is malformed"))?;
    if document.roots.len() > ROOT_CHAIN_LIMIT {
        return Err(miette!("signed update root chain has an invalid length"));
    }
    validate_root_entries(&document.roots)?;
    Ok(document.roots)
}

fn decode_root_entries(entries: &[RootChainEntry]) -> Result<Vec<Vec<u8>>> {
    let mut total = 0_usize;
    entries
        .iter()
        .map(|entry| {
            if entry.envelope.len() > METADATA_LIMIT {
                return Err(miette!("signed update root envelope is oversized"));
            }
            let envelope = STANDARD
                .decode(entry.envelope.as_bytes())
                .map_err(|_| miette!("signed update root envelope is malformed"))?;
            total = total.saturating_add(envelope.len());
            if envelope.is_empty() || total > METADATA_LIMIT {
                return Err(miette!("signed update root chain is oversized"));
            }
            Ok(envelope)
        })
        .collect()
}

fn validate_root_entries(entries: &[RootChainEntry]) -> Result<()> {
    if entries.len() > ROOT_CHAIN_LIMIT
        || entries.iter().any(|entry| entry.version == 0)
        || entries
            .windows(2)
            .any(|pair| pair[1].version != pair[0].version.saturating_add(1))
    {
        return Err(miette!("signed update root chain is not sequential"));
    }
    Ok(())
}

fn merge_root_chains(
    persisted: &[RootChainEntry],
    successors: &[RootChainEntry],
) -> Result<Vec<RootChainEntry>> {
    let mut merged = BTreeMap::new();
    for entry in persisted.iter().chain(successors) {
        if let Some(existing) = merged.insert(entry.version, entry.clone())
            && existing.envelope != entry.envelope
        {
            return Err(miette!(
                "signed update root chain contains conflicting versions"
            ));
        }
    }
    let merged = merged.into_values().collect::<Vec<_>>();
    validate_root_entries(&merged)?;
    Ok(merged)
}

fn restore_and_select_roots(
    embedded: &TrustedRoot,
    expected_version: u64,
    persisted: &[RootChainEntry],
    fetched: Vec<RootChainEntry>,
) -> Result<RootSelection> {
    let persisted = persisted
        .iter()
        .filter(|entry| entry.version >= embedded.version())
        .cloned()
        .collect::<Vec<_>>();
    let persisted_bytes = decode_root_entries(&persisted)?;
    let persisted_slices = persisted_bytes
        .iter()
        .map(Vec::as_slice)
        .collect::<Vec<_>>();
    let trusted = restore_trusted_root_chain(embedded, &persisted_slices).into_diagnostic()?;
    if expected_version > 0 && trusted.version() != expected_version {
        return Err(miette!(
            "durable update root state does not match its high-water mark"
        ));
    }
    let include_current = persisted.is_empty();
    let successors = fetched
        .into_iter()
        .filter(|entry| {
            entry.version > trusted.version()
                || (include_current && entry.version == trusted.version())
        })
        .collect::<Vec<_>>();
    validate_root_entries(&successors)?;
    let successor_envelopes = decode_root_entries(&successors)?;
    let accepted_chain = merge_root_chains(&persisted, &successors)?;
    Ok(RootSelection {
        trusted,
        successor_envelopes,
        accepted_chain,
    })
}

fn embedded_update_base_url() -> Result<Url> {
    let value = EMBEDDED_UPDATE_BASE_URL
        .ok_or_else(|| miette!("this build has no embedded update metadata origin"))?;
    validate_embedded_update_base_url(value)
}

fn validate_embedded_update_base_url(value: &str) -> Result<Url> {
    let url =
        Url::parse(value).map_err(|_| miette!("compiled update metadata origin is invalid"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
    {
        return Err(miette!("compiled update metadata origin is invalid"));
    }
    Ok(url)
}

fn release_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        _ => "unsupported",
    }
}

fn expected_generation_files() -> BTreeMap<&'static str, (u64, u32)> {
    let native = if cfg!(target_os = "macos") {
        "bin/libopentui.dylib"
    } else {
        "bin/libopentui.so"
    };
    BTreeMap::from([
        ("install.sh", (128 * 1024, 0o755)),
        ("bin/rw", (25 * 1024 * 1024, 0o755)),
        ("bin/rottweiler-tui", (100 * 1024 * 1024, 0o755)),
        (native, (100 * 1024 * 1024, 0o644)),
    ])
}

async fn stage_generation(
    layout: &InstallLayout,
    verified: &VerifiedUpdate,
    artifact: &[u8],
) -> Result<Generation> {
    let nonce = nonce()?;
    let staging = layout.versions.join(format!(".upgrade-staging-{nonce}"));
    fs::create_dir(&staging).into_diagnostic()?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).into_diagnostic()?;
    let guard = StagingGuard(staging.clone());
    extract_exact_archive(
        &staging,
        &verified.version().to_string(),
        verified.platform(),
        artifact,
    )?;
    validate_staged_binary(&staging.join("bin/rw"), &verified.version().to_string()).await?;
    let generation = inspect_generation(
        &staging,
        &verified.version().to_string(),
        verified.platform(),
    )?;
    let destination = layout.versions.join(&generation.version);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let existing =
                inspect_generation(&destination, &generation.version, &generation.platform)?;
            if existing != generation {
                return Err(miette!(
                    "an existing update generation differs from the signed artifact"
                ));
            }
        }
        Ok(_) => return Err(miette!("update generation path is unsafe")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(&staging, &destination).into_diagnostic()?;
            sync_directory(&layout.versions)?;
        }
        Err(error) => return Err(error).into_diagnostic(),
    }
    drop(guard);
    Ok(generation)
}

#[allow(clippy::too_many_lines)]
fn extract_exact_archive(
    staging: &Path,
    version: &str,
    platform: &str,
    artifact: &[u8],
) -> Result<()> {
    let root = format!("rottweiler-{version}-{platform}");
    let files = expected_generation_files();
    let expected = [root.clone(), format!("{root}/bin")]
        .into_iter()
        .chain(files.keys().map(|path| format!("{root}/{path}")))
        .collect::<BTreeSet<_>>();
    let decoder = GzDecoder::new(Cursor::new(artifact));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|_| miette!("signed update archive is malformed"))?;
    let mut seen = BTreeSet::new();
    let mut expanded = 0_u64;
    for entry in entries {
        let mut entry = entry.map_err(|_| miette!("signed update archive is malformed"))?;
        if seen.len() >= ARCHIVE_ENTRY_LIMIT {
            return Err(miette!("signed update archive has too many entries"));
        }
        let path = entry
            .path()
            .map_err(|_| miette!("signed update archive path is invalid"))?
            .into_owned();
        if !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(miette!("signed update archive path is unsafe"));
        }
        let path_text = path
            .to_str()
            .ok_or_else(|| miette!("signed update archive path is not UTF-8"))?
            .to_owned();
        if !expected.contains(&path_text) || !seen.insert(path_text.clone()) {
            return Err(miette!(
                "signed update archive entry is unexpected or duplicated"
            ));
        }
        let entry_type = entry.header().entry_type();
        if path_text == root || path_text == format!("{root}/bin") {
            if entry_type != EntryType::Directory {
                return Err(miette!("signed update archive directory entry is invalid"));
            }
            if path_text != root {
                let bin = staging.join("bin");
                fs::create_dir_all(&bin).into_diagnostic()?;
                fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).into_diagnostic()?;
            }
            continue;
        }
        if entry_type != EntryType::Regular {
            return Err(miette!(
                "signed update archive contains a link or special file"
            ));
        }
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| miette!("signed update archive root is invalid"))?;
        let relative_text = relative
            .to_str()
            .ok_or_else(|| miette!("signed update archive path is not UTF-8"))?;
        let (limit, mode) = files
            .get(relative_text)
            .copied()
            .ok_or_else(|| miette!("signed update archive file is not allowlisted"))?;
        let size = entry
            .header()
            .size()
            .map_err(|_| miette!("signed update archive size is invalid"))?;
        expanded = expanded.saturating_add(size);
        if size == 0 || size > limit || expanded > EXPANDED_LIMIT {
            return Err(miette!("signed update archive expanded size is invalid"));
        }
        let destination = staging.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).into_diagnostic()?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&destination)
            .into_diagnostic()?;
        let copied = std::io::copy(
            &mut entry.by_ref().take(size.saturating_add(1)),
            &mut output,
        )
        .into_diagnostic()?;
        if copied != size {
            return Err(miette!("signed update archive file length changed"));
        }
        output.sync_all().into_diagnostic()?;
        output
            .set_permissions(fs::Permissions::from_mode(mode))
            .into_diagnostic()?;
        output.sync_all().into_diagnostic()?;
    }
    if seen != expected {
        return Err(miette!("signed update archive is missing required entries"));
    }
    sync_directory(staging)?;
    sync_directory(&staging.join("bin"))?;
    Ok(())
}

async fn validate_staged_binary(path: &Path, expected_version: &str) -> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(path)
            .arg("--version")
            .env_clear()
            .output(),
    )
    .await
    .map_err(|_| miette!("staged update binary version check timed out"))?
    .into_diagnostic()?;
    if !output.status.success()
        || output.stdout.len() > 4096
        || output.stderr.len() > 4096
        || String::from_utf8_lossy(&output.stdout).trim() != format!("rw {expected_version}")
    {
        return Err(miette!(
            "staged update binary did not report the signed version"
        ));
    }
    Ok(())
}

fn inspect_generation(path: &Path, version: &str, platform: &str) -> Result<Generation> {
    validate_managed_directory(path, "generation")?;
    let expected = expected_generation_files();
    let mut observed = BTreeSet::new();
    for entry in fs::read_dir(path).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| miette!("generation contains a non-UTF-8 entry"))?;
        observed.insert(name.to_owned());
    }
    if observed != BTreeSet::from(["bin".to_owned(), "install.sh".to_owned()]) {
        return Err(miette!("generation contains unexpected entries"));
    }
    let bin = path.join("bin");
    validate_managed_directory(&bin, "generation bin directory")?;
    let expected_bin = expected
        .keys()
        .filter_map(|value| value.strip_prefix("bin/"))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let observed_bin = fs::read_dir(&bin)
        .into_diagnostic()?
        .map(|entry| {
            entry
                .map_err(|_| miette!("generation bin directory could not be read"))?
                .file_name()
                .into_string()
                .map_err(|_| miette!("generation contains a non-UTF-8 entry"))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if observed_bin != expected_bin {
        return Err(miette!("generation contains unexpected runtime files"));
    }
    let files = expected
        .iter()
        .map(|(relative, (limit, mode))| {
            let file = path.join(relative);
            Ok((
                (*relative).to_owned(),
                blake3_managed_file(&file, *limit, *mode)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(Generation {
        version: version.to_owned(),
        platform: platform.to_owned(),
        files,
    })
}

fn load_or_bootstrap_state(layout: &InstallLayout) -> Result<UpgradeState> {
    match fs::symlink_metadata(layout.prefix.join("update-state.json")) {
        Ok(_) => load_state(layout),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::symlink_metadata(layout.prefix.join(STATE_MARKER)) {
                Ok(_) => {
                    return Err(miette!(
                        "durable update state was deleted after initialization"
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).into_diagnostic(),
            }
            Ok(UpgradeState {
                schema_version: 1,
                highest_root_version: 0,
                highest_metadata_version: 0,
                trusted_unix_time: 0,
                trusted_root_chain: Vec::new(),
                active: inspect_generation(
                    &layout.versions.join(&layout.current_version),
                    &layout.current_version,
                    release_platform(),
                )?,
                previous: None,
                pending_release_notes: None,
            })
        }
        Err(error) => Err(error).into_diagnostic(),
    }
}

fn load_state(layout: &InstallLayout) -> Result<UpgradeState> {
    let state = load_state_document(layout)?;
    validate_layout_state(layout, &state)?;
    Ok(state)
}

fn load_state_document(layout: &InstallLayout) -> Result<UpgradeState> {
    let path = layout.prefix.join("update-state.json");
    let bytes = read_private_state_file(&path, "update state file")?;
    let state: UpgradeState =
        serde_json::from_slice(&bytes).map_err(|_| miette!("update state file is malformed"))?;
    validate_state(&state)?;
    Ok(state)
}

fn validate_layout_state(layout: &InstallLayout, state: &UpgradeState) -> Result<()> {
    if state.active.version != layout.current_version {
        return Err(miette!(
            "active update state does not match the selected generation"
        ));
    }
    let observed = inspect_generation(
        &layout.versions.join(&layout.current_version),
        &layout.current_version,
        release_platform(),
    )?;
    if observed != state.active {
        return Err(miette!(
            "active generation failed its local integrity check"
        ));
    }
    Ok(())
}

fn validate_state(state: &UpgradeState) -> Result<()> {
    validate_root_entries(&state.trusted_root_chain)?;
    if state.schema_version != 1
        || !safe_selector(&state.active.version)
        || state.active.platform != release_platform()
        || state.trusted_root_chain.len() > ROOT_CHAIN_LIMIT
        || state
            .trusted_root_chain
            .iter()
            .any(|entry| entry.envelope.len() > METADATA_LIMIT)
        || (state.highest_root_version > 0
            && state.trusted_root_chain.last().map(|entry| entry.version)
                != Some(state.highest_root_version))
        || (state.highest_root_version == 0 && !state.trusted_root_chain.is_empty())
        || state.previous.as_ref().is_some_and(|value| {
            !safe_selector(&value.version) || value.platform != release_platform()
        })
        || state.pending_release_notes.as_ref().is_some_and(|notes| {
            notes.notes.len() > 64 * 1024
                || notes
                    .notes
                    .chars()
                    .any(|value| value.is_control() && !matches!(value, '\n' | '\r' | '\t'))
        })
    {
        return Err(miette!("update state file is invalid"));
    }
    Ok(())
}

fn advance_high_water(state: &mut UpgradeState, verified: &VerifiedUpdate) {
    state.highest_root_version = state.highest_root_version.max(verified.root_version());
    state.highest_metadata_version = state
        .highest_metadata_version
        .max(verified.metadata_version());
    state.trusted_unix_time = state.trusted_unix_time.max(verified.trusted_unix_time());
}

fn commit_activation(layout: &InstallLayout, next: &UpgradeState) -> Result<()> {
    let pending = layout.prefix.join("update-state.pending.json");
    write_json_file_atomic(&pending, next)?;
    activate_generation(layout, &next.active.version)?;
    fs::rename(&pending, layout.prefix.join("update-state.json")).into_diagnostic()?;
    ensure_state_marker(layout)?;
    sync_directory(&layout.prefix)
}

fn recover_pending_state(layout: &InstallLayout) -> Result<()> {
    let pending = layout.prefix.join("update-state.pending.json");
    let Ok(metadata) = fs::symlink_metadata(&pending) else {
        return Ok(());
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > STATE_LIMIT
    {
        return Err(miette!("pending update journal is unsafe"));
    }
    let bytes = read_private_state_file(&pending, "pending update journal")?;
    let state: UpgradeState = serde_json::from_slice(&bytes)
        .map_err(|_| miette!("pending update journal is malformed"))?;
    validate_state(&state)?;
    let selected = fs::read_link(layout.prefix.join("current")).into_diagnostic()?;
    if selected == PathBuf::from("versions").join(&state.active.version) {
        validate_layout_state(layout, &state)?;
        fs::rename(&pending, layout.prefix.join("update-state.json")).into_diagnostic()?;
        ensure_state_marker(layout)?;
    } else {
        fs::remove_file(&pending).into_diagnostic()?;
    }
    sync_directory(&layout.prefix)
}

fn rollback(layout: &InstallLayout, state: &mut UpgradeState) -> Result<()> {
    let previous = state
        .previous
        .clone()
        .ok_or_else(|| miette!("no previous signed generation is available"))?;
    let observed = inspect_generation(
        &layout.versions.join(&previous.version),
        &previous.version,
        &previous.platform,
    )?;
    if observed != previous {
        return Err(miette!(
            "previous generation failed its local integrity check"
        ));
    }
    let next = UpgradeState {
        active: previous,
        previous: Some(state.active.clone()),
        pending_release_notes: None,
        ..state.clone()
    };
    commit_activation(layout, &next)?;
    *state = next;
    Ok(())
}

fn activate_generation(layout: &InstallLayout, version: &str) -> Result<()> {
    if !safe_selector(version) || !layout.versions.join(version).is_dir() {
        return Err(miette!("update generation selector is invalid"));
    }
    let temporary = layout.prefix.join(format!(".current-{}", nonce()?));
    symlink(PathBuf::from("versions").join(version), &temporary).into_diagnostic()?;
    fs::rename(&temporary, layout.prefix.join("current")).into_diagnostic()?;
    sync_directory(&layout.prefix)
}

fn write_state_atomic(layout: &InstallLayout, state: &UpgradeState) -> Result<()> {
    write_json_file_atomic(&layout.prefix.join("update-state.json"), state)?;
    ensure_state_marker(layout)
}

fn write_json_file_atomic(path: &Path, state: &UpgradeState) -> Result<()> {
    validate_state(state)?;
    let bytes = serde_json::to_vec(state).into_diagnostic()?;
    if bytes.len() as u64 > STATE_LIMIT {
        return Err(miette!("update state exceeded its size limit"));
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink() || metadata.nlink() != 1)
    {
        return Err(miette!("update state destination is unsafe"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| miette!("update state has no parent directory"))?;
    let temporary = parent.join(format!(".update-state-{}", nonce()?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .into_diagnostic()?;
    file.write_all(&bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()?;
    fs::rename(&temporary, path).into_diagnostic()?;
    sync_directory(parent)
}

fn ensure_state_marker(layout: &InstallLayout) -> Result<()> {
    let path = layout.prefix.join(STATE_MARKER);
    match fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.nlink() == 1
                && metadata.mode() & 0o777 == 0o600 =>
        {
            Ok(())
        }
        Ok(_) => Err(miette!("update state marker is unsafe")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .into_diagnostic()?;
            marker.write_all(b"1\n").into_diagnostic()?;
            marker.sync_all().into_diagnostic()?;
            sync_directory(&layout.prefix)
        }
        Err(error) => Err(error).into_diagnostic(),
    }
}

fn read_private_state_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    read_private_file(path, STATE_LIMIT, label)
}

fn read_private_file(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let mut file = File::from(descriptor);
    let before = file.metadata().into_diagnostic()?;
    if !before.is_file()
        || before.nlink() != 1
        || before.mode() & 0o777 != 0o600
        || before.uid() != rustix::process::geteuid().as_raw()
        || before.len() > limit
    {
        return Err(miette!("{label} is unsafe"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
    std::io::Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    let after = file.metadata().into_diagnostic()?;
    if bytes.len() as u64 > limit
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(miette!("{label} changed while it was read"));
    }
    Ok(bytes)
}

fn refuse_wsl_drvfs(prefix: &Path) -> Result<()> {
    if !running_under_wsl() {
        return Ok(());
    }
    let mounts = bounded_read(Path::new("/proc/mounts"), MOUNTS_LIMIT).unwrap_or_default();
    if path_is_drvfs(prefix, &String::from_utf8_lossy(&mounts))
        || (mounts.is_empty() && looks_like_wsl_drive_path(prefix))
    {
        return Err(miette!(
            "self-update refuses a WSL DrvFS install; reinstall inside the Linux filesystem"
        ));
    }
    Ok(())
}

fn running_under_wsl() -> bool {
    std::env::var_os("WSL_INTEROP").is_some()
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || bounded_read(Path::new("/proc/sys/kernel/osrelease"), OS_RELEASE_LIMIT).is_some_and(
            |bytes| {
                String::from_utf8_lossy(&bytes)
                    .to_ascii_lowercase()
                    .contains("microsoft")
            },
        )
}

fn path_is_drvfs(path: &Path, mounts: &str) -> bool {
    mounts
        .lines()
        .filter_map(|line| {
            let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
            (fields.len() >= 4).then(|| {
                (
                    PathBuf::from(unescape_mount_field(fields[1])),
                    fields[2],
                    fields[3],
                )
            })
        })
        .filter(|(mount, _, _)| path.starts_with(mount))
        .max_by_key(|(mount, _, _)| mount.components().count())
        .is_some_and(|(_, kind, options)| {
            kind.eq_ignore_ascii_case("drvfs")
                || (kind == "9p" && options.to_ascii_lowercase().contains("aname=drvfs"))
        })
}

fn unescape_mount_field(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\134", "\\")
}

fn looks_like_wsl_drive_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::RootDir))
        && components
            .next()
            .and_then(|value| value.as_os_str().to_str())
            == Some("mnt")
        && components.next().is_some_and(|value| {
            value
                .as_os_str()
                .to_str()
                .is_some_and(|value| value.len() == 1 && value.as_bytes()[0].is_ascii_alphabetic())
        })
}

fn bounded_read(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

fn validate_managed_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).into_diagnostic()?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err(miette!("{label} is not a safe managed directory"));
    }
    Ok(())
}

fn safe_selector(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

fn blake3_managed_file(path: &Path, limit: u64, expected_mode: u32) -> Result<String> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let mut file = File::from(descriptor);
    let before = file.metadata().into_diagnostic()?;
    if !before.is_file()
        || before.nlink() != 1
        || before.uid() != rustix::process::geteuid().as_raw()
        || before.mode() & 0o777 != expected_mode
        || before.len() == 0
        || before.len() > limit
    {
        return Err(miette!("generation runtime file is unsafe"));
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = file.read(&mut buffer).into_diagnostic()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata().into_diagnostic()?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(miette!("generation runtime file changed while it was read"));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn nonce() -> Result<String> {
    use std::fmt::Write as _;

    let mut bytes = [0_u8; 12];
    getrandom::fill(&mut bytes).into_diagnostic()?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(24), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        }))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .into_diagnostic()?
        .sync_all()
        .into_diagnostic()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};
    use flate2::{Compression, write::GzEncoder};
    use serde_json::json;
    use tar::{Builder, Header};

    use super::*;

    fn sign_envelope(
        role: &str,
        payload: &serde_json::Value,
        keys: &[(&str, &SigningKey)],
    ) -> Vec<u8> {
        let payload = serde_json::to_vec(payload).expect("payload");
        let mut message = b"rottweiler-update-metadata-v1\0".to_vec();
        message.extend_from_slice(role.as_bytes());
        message.push(0);
        message.extend_from_slice(&payload);
        serde_json::to_vec(&json!({
            "payload": STANDARD.encode(&payload),
            "signatures": keys.iter().map(|(id, key)| json!({
                "key_id": id,
                "signature": STANDARD.encode(key.sign(&message).to_bytes()),
            })).collect::<Vec<_>>(),
        }))
        .expect("envelope")
    }

    fn signed_root_entry(
        version: u64,
        expires: u64,
        root_id: &str,
        root_key: &SigningKey,
        release_id: &str,
        release_key: &SigningKey,
        signers: &[(&str, &SigningKey)],
    ) -> RootChainEntry {
        let envelope = sign_envelope(
            "root",
            &json!({
                "schema_version": 1,
                "role": "root",
                "version": version,
                "expires_unix": expires,
                "keys": {
                    (root_id): STANDARD.encode(root_key.verifying_key().to_bytes()),
                    (release_id): STANDARD.encode(release_key.verifying_key().to_bytes()),
                },
                "root_key_ids": [root_id],
                "root_threshold": 1,
                "release_key_ids": [release_id],
                "release_threshold": 1,
            }),
            signers,
        );
        RootChainEntry {
            version,
            envelope: STANDARD.encode(envelope),
        }
    }

    fn archive_fixture(link_rw: bool, unexpected: bool) -> Vec<u8> {
        let root = format!("rottweiler-1.2.3-{}", release_platform());
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        for directory in [&root, &format!("{root}/bin")] {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_data(&mut header, directory, std::io::empty())
                .expect("directory fixture");
        }
        let native = if cfg!(target_os = "macos") {
            "libopentui.dylib"
        } else {
            "libopentui.so"
        };
        for (relative, bytes) in [
            ("install.sh", b"#!/bin/sh\n".as_slice()),
            ("bin/rottweiler-tui", b"tui".as_slice()),
            ("bin/native-placeholder", b"native".as_slice()),
        ] {
            let relative = if relative == "bin/native-placeholder" {
                format!("bin/{native}")
            } else {
                relative.to_owned()
            };
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_mode(0o755);
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{root}/{relative}"), bytes)
                .expect("file fixture");
        }
        let mut rw_header = Header::new_gnu();
        if link_rw {
            rw_header.set_entry_type(EntryType::Symlink);
            rw_header
                .set_link_name("../../outside")
                .expect("link target");
            rw_header.set_size(0);
        } else {
            rw_header.set_entry_type(EntryType::Regular);
            rw_header.set_size(2);
        }
        rw_header.set_mode(0o755);
        rw_header.set_cksum();
        builder
            .append_data(
                &mut rw_header,
                format!("{root}/bin/rw"),
                if link_rw {
                    b"".as_slice()
                } else {
                    b"rw".as_slice()
                },
            )
            .expect("rw fixture");
        if unexpected {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(4);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{root}/extra"), b"evil".as_slice())
                .expect("unexpected fixture");
        }
        let encoder = builder.into_inner().expect("tar fixture");
        encoder.finish().expect("gzip fixture")
    }

    #[test]
    fn drvfs_mount_detection_is_longest_prefix_and_wsl_specific() {
        let mounts = "none /mnt/c 9p rw,dirsync,aname=drvfs 0 0\n/dev/sda / ext4 rw 0 0\n";
        assert!(path_is_drvfs(Path::new("/mnt/c/Users/test/rw"), mounts));
        assert!(!path_is_drvfs(Path::new("/home/test/rw"), mounts));
        assert!(looks_like_wsl_drive_path(Path::new("/mnt/d/tools")));
        assert!(!looks_like_wsl_drive_path(Path::new("/home/tools")));
    }

    #[test]
    fn unknown_or_package_managed_layout_is_refused() {
        let root = tempfile::tempdir().expect("root");
        let direct = root.path().join("rw");
        fs::write(&direct, b"binary").expect("fixture");
        assert!(InstallLayout::from_executable(&direct).is_err());
    }

    #[test]
    fn unsupported_homebrew_layout_preserves_refusal_with_package_guidance() {
        let homebrew = unsupported_layout_for(Some(std::ffi::OsStr::new("homebrew"))).to_string();
        assert!(homebrew.contains("brew upgrade rottweiler"));
        assert!(homebrew.contains("never modifies package-managed files"));

        let unknown = unsupported_layout_for(Some(std::ffi::OsStr::new("other"))).to_string();
        assert!(unknown.contains("official versioned installation layout"));
        assert!(!unknown.contains("brew upgrade"));
    }

    #[test]
    fn embedded_update_base_url_requires_repository_trailing_slash() {
        assert!(validate_embedded_update_base_url("https://updates.example/v1/").is_ok());
        assert!(validate_embedded_update_base_url("https://updates.example/v1").is_err());
        assert!(validate_embedded_update_base_url("https://updates.example/v1/?x=1").is_err());
    }

    #[test]
    fn upgrade_lock_refuses_live_owner_and_recovers_dead_owner() {
        let live_root = tempfile::tempdir().expect("live root");
        let live = UpgradeLock::acquire(live_root.path()).expect("live lock");
        assert!(UpgradeLock::acquire(live_root.path()).is_err());
        drop(live);

        let stale_root = tempfile::tempdir().expect("stale root");
        let stale = stale_root.path().join(".install-lock");
        fs::create_dir(&stale).expect("stale lock");
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o700)).expect("stale mode");
        let owner = stale.join("pid");
        fs::write(&owner, b"2147483647\n").expect("stale owner");
        fs::set_permissions(&owner, fs::Permissions::from_mode(0o600)).expect("owner mode");
        let recovered = UpgradeLock::acquire(stale_root.path()).expect("recover stale lock");
        assert_eq!(
            fs::read_to_string(stale.join("pid"))
                .expect("new owner")
                .trim(),
            std::process::id().to_string()
        );
        drop(recovered);
    }

    #[test]
    fn managed_layout_requires_selected_generation_and_launcher() {
        let root = tempfile::tempdir().expect("root");
        let generation = root.path().join("versions/1.2.3/bin");
        fs::create_dir_all(&generation).expect("generation");
        let executable = generation.join("rw");
        fs::write(&executable, b"binary").expect("fixture");
        fs::create_dir(root.path().join("bin")).expect("bin");
        symlink("versions/1.2.3", root.path().join("current")).expect("current");
        symlink("../current/bin/rw", root.path().join("bin/rw")).expect("launcher");
        let layout = InstallLayout::from_executable(&executable).expect("managed layout");
        assert_eq!(layout.current_version, "1.2.3");
    }

    #[test]
    fn update_state_rejects_control_characters_and_unsafe_selectors() {
        let generation = Generation {
            version: "1.2.3".to_owned(),
            platform: release_platform().to_owned(),
            files: BTreeMap::new(),
        };
        let mut state = UpgradeState {
            schema_version: 1,
            highest_root_version: 1,
            highest_metadata_version: 1,
            trusted_unix_time: 1,
            trusted_root_chain: Vec::new(),
            active: generation,
            previous: None,
            pending_release_notes: Some(PendingReleaseNotes {
                version: "1.2.3".to_owned(),
                notes: "bad\u{0007}".to_owned(),
            }),
        };
        assert!(validate_state(&state).is_err());
        state.pending_release_notes = None;
        state.active.version = "../escape".to_owned();
        assert!(validate_state(&state).is_err());
    }

    #[test]
    fn initialized_state_marker_makes_state_deletion_fail_closed() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join(STATE_MARKER), b"1\n").expect("marker");
        let layout = InstallLayout {
            prefix: root.path().to_path_buf(),
            versions: root.path().join("versions"),
            current_version: "1.2.3".to_owned(),
        };
        assert!(load_or_bootstrap_state(&layout).is_err());
    }

    #[test]
    fn state_active_version_must_match_current_selector() {
        let root = tempfile::tempdir().expect("root");
        let layout = InstallLayout {
            prefix: root.path().to_path_buf(),
            versions: root.path().join("versions"),
            current_version: "1.2.3".to_owned(),
        };
        let state = UpgradeState {
            schema_version: 1,
            highest_root_version: 0,
            highest_metadata_version: 0,
            trusted_unix_time: 0,
            trusted_root_chain: Vec::new(),
            active: Generation {
                version: "9.9.9".to_owned(),
                platform: release_platform().to_owned(),
                files: BTreeMap::new(),
            },
            previous: None,
            pending_release_notes: None,
        };
        assert!(validate_layout_state(&layout, &state).is_err());
    }

    #[test]
    fn exact_archive_allowlist_extracts_without_links_or_extra_entries() {
        let root = tempfile::tempdir().expect("root");
        let staging = root.path().join("staging");
        fs::create_dir(&staging).expect("staging");
        extract_exact_archive(
            &staging,
            "1.2.3",
            release_platform(),
            &archive_fixture(false, false),
        )
        .expect("exact archive");
        assert_eq!(fs::read(staging.join("bin/rw")).expect("rw"), b"rw");
        assert!(
            fs::symlink_metadata(staging.join("bin/rw"))
                .expect("metadata")
                .is_file()
        );
    }

    #[test]
    fn archive_links_and_unexpected_entries_fail_closed_in_staging() {
        for artifact in [archive_fixture(true, false), archive_fixture(false, true)] {
            let root = tempfile::tempdir().expect("root");
            let staging = root.path().join("staging");
            fs::create_dir(&staging).expect("staging");
            assert!(
                extract_exact_archive(&staging, "1.2.3", release_platform(), &artifact).is_err()
            );
            assert!(!root.path().join("outside").exists());
        }
    }

    #[test]
    fn persisted_v3_trust_accepts_v4_after_historical_v2_expiry() {
        let first = SigningKey::from_bytes(&[31; 32]);
        let second = SigningKey::from_bytes(&[32; 32]);
        let third = SigningKey::from_bytes(&[33; 32]);
        let fourth = SigningKey::from_bytes(&[34; 32]);
        let second_release = SigningKey::from_bytes(&[35; 32]);
        let third_release = SigningKey::from_bytes(&[36; 32]);
        let fourth_release = SigningKey::from_bytes(&[37; 32]);
        let embedded = TrustedRoot::from_keys(
            1,
            1,
            [("root-1".to_owned(), first.verifying_key().to_bytes())],
        )
        .expect("embedded root");
        let v2 = signed_root_entry(
            2,
            101,
            "root-2",
            &second,
            "release-2",
            &second_release,
            &[("root-1", &first), ("root-2", &second)],
        );
        let v3 = signed_root_entry(
            3,
            1_000,
            "root-3",
            &third,
            "release-3",
            &third_release,
            &[("root-2", &second), ("root-3", &third)],
        );
        let v4 = signed_root_entry(
            4,
            1_000,
            "root-4",
            &fourth,
            "release-4",
            &fourth_release,
            &[("root-3", &third), ("root-4", &fourth)],
        );
        let selection = restore_and_select_roots(&embedded, 3, &[v2, v3], vec![v4])
            .expect("restore v3 and select v4");
        assert_eq!(selection.trusted.version(), 3);
        let release = sign_envelope(
            "release",
            &json!({
                "schema_version": 1,
                "role": "release",
                "version": 7,
                "expires_unix": 900,
                "channel": "stable",
                "release_notes": "v4",
                "targets": {
                    (release_platform()): {
                        "version": "1.1.0",
                        "url": "https://release.example.invalid/v4.tar.gz",
                        "length": 1,
                        "sha256": "00".repeat(32),
                    }
                }
            }),
            &[("release-4", &fourth_release)],
        );
        let successor_slices = selection
            .successor_envelopes
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        assert!(
            verify_update_metadata_chain(
                &selection.trusted,
                &successor_slices,
                &release,
                &UpdateVerificationPolicy {
                    channel: UpdateChannel::Stable,
                    platform: release_platform(),
                    current_version: "1.0.0",
                    now_unix: 200,
                    high_water: UpdateHighWaterMark {
                        root_version: 3,
                        metadata_version: 6,
                        trusted_unix_time: 100,
                    },
                    allow_downgrade: false,
                },
            )
            .is_ok()
        );
    }
}
