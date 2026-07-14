//! Signed extension-registry catalogs and verified local installation.

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::{MAX_NAME_BYTES, PluginManifest};

const REGISTRY_SCHEMA: u32 = 1;
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_RELEASES: usize = 10_000;
const MAX_COMPONENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENABLED_EXTENSIONS: usize = 32;
const MAX_ENABLED_COMPONENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_RELEASE_RECORD_BYTES: usize = 384 * 1024;
const MAX_TRUST_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const ACTIVATION_FILE: &str = "enabled.json";
const RELEASE_FILE: &str = "release.json";
const TRUSTED_PUBLISHERS_FILE: &str = "trusted-publishers.json";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryArtifact {
    pub url: String,
    pub blake3: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryRelease {
    pub name: String,
    pub version: String,
    pub manifest: PluginManifest,
    pub component: RegistryArtifact,
    pub publisher_key: String,
    pub signature: String,
}

#[derive(Serialize)]
struct UnsignedRelease<'a> {
    name: &'a str,
    version: &'a str,
    manifest: &'a PluginManifest,
    component: &'a RegistryArtifact,
    publisher_key: &'a str,
}

impl RegistryRelease {
    fn validate_metadata(&self) -> Result<(), RegistryError> {
        self.manifest
            .validate()
            .map_err(|error| RegistryError::InvalidManifest {
                message: error.to_string(),
            })?;
        if self.name != self.manifest.name || self.version != self.manifest.version {
            return Err(RegistryError::IdentityMismatch);
        }
        Version::parse(&self.version).map_err(|_| RegistryError::InvalidVersion)?;
        let url = Url::parse(&self.component.url).map_err(|_| RegistryError::InvalidUrl)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(RegistryError::InvalidUrl);
        }
        if self.component.size == 0 || self.component.size > MAX_COMPONENT_BYTES as u64 {
            return Err(RegistryError::InvalidArtifactSize);
        }
        if !is_lowercase_digest(&self.component.blake3) {
            return Err(RegistryError::InvalidDigest);
        }
        decode_exact::<32>(&self.publisher_key).map_err(|()| RegistryError::InvalidPublisherKey)?;
        decode_exact::<64>(&self.signature).map_err(|()| RegistryError::InvalidSignature)?;
        Ok(())
    }

    /// Stable bytes covered by the publisher signature.
    ///
    /// # Errors
    /// Returns an error if the release metadata cannot be encoded.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, RegistryError> {
        serde_json::to_vec(&UnsignedRelease {
            name: &self.name,
            version: &self.version,
            manifest: &self.manifest,
            component: &self.component,
            publisher_key: &self.publisher_key,
        })
        .map_err(|error| RegistryError::Malformed {
            message: error.to_string(),
        })
    }

    /// Validates metadata, signature, and the caller's separately pinned key.
    ///
    /// # Errors
    /// Returns an error for invalid metadata, untrusted keys, or bad signatures.
    pub fn verify(&self, trusted_publisher_key: &[u8; 32]) -> Result<(), RegistryError> {
        self.validate_metadata()?;
        let declared_key = decode_exact::<32>(&self.publisher_key)
            .map_err(|()| RegistryError::InvalidPublisherKey)?;
        if &declared_key != trusted_publisher_key {
            return Err(RegistryError::UntrustedPublisher);
        }
        let verifying_key = VerifyingKey::from_bytes(trusted_publisher_key)
            .map_err(|_| RegistryError::InvalidPublisherKey)?;
        let signature_bytes =
            decode_exact::<64>(&self.signature).map_err(|()| RegistryError::InvalidSignature)?;
        verifying_key
            .verify(
                &self.signing_bytes()?,
                &Signature::from_bytes(&signature_bytes),
            )
            .map_err(|_| RegistryError::InvalidSignature)
    }

    /// Verifies downloaded component bytes against the signed release.
    ///
    /// # Errors
    /// Returns an error when the signed size or digest does not match.
    pub fn verify_component(&self, bytes: &[u8]) -> Result<(), RegistryError> {
        if usize::try_from(self.component.size).ok() != Some(bytes.len())
            || bytes.len() > MAX_COMPONENT_BYTES
        {
            return Err(RegistryError::ArtifactSizeMismatch);
        }
        if blake3::hash(bytes).to_hex().as_str() != self.component.blake3 {
            return Err(RegistryError::ArtifactDigestMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionRegistryCatalog {
    pub schema: u32,
    pub releases: Vec<RegistryRelease>,
}

impl ExtensionRegistryCatalog {
    /// Parses a bounded cache snapshot. A snapshot is never trusted until the
    /// selected release is verified against a separately pinned publisher key.
    ///
    /// # Errors
    /// Returns an error for oversized, malformed, duplicate, or unknown-schema catalogs.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, RegistryError> {
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(RegistryError::CatalogTooLarge);
        }
        let catalog: Self =
            serde_json::from_slice(bytes).map_err(|error| RegistryError::Malformed {
                message: error.to_string(),
            })?;
        if catalog.schema != REGISTRY_SCHEMA {
            return Err(RegistryError::UnsupportedSchema {
                schema: catalog.schema,
            });
        }
        if catalog.releases.len() > MAX_RELEASES {
            return Err(RegistryError::TooManyReleases);
        }
        let mut identities = BTreeSet::new();
        for release in &catalog.releases {
            release.validate_metadata()?;
            if !identities.insert((&release.name, &release.version)) {
                return Err(RegistryError::DuplicateRelease {
                    name: release.name.clone(),
                    version: release.version.clone(),
                });
            }
        }
        Ok(catalog)
    }

    /// Resolves the greatest semantic version for a name. Catalog order never
    /// influences selection.
    #[must_use]
    pub fn latest(&self, name: &str) -> Option<&RegistryRelease> {
        self.releases
            .iter()
            .filter(|release| release.name == name)
            .filter_map(|release| Version::parse(&release.version).ok().map(|v| (v, release)))
            .max_by(|left, right| left.0.cmp(&right.0))
            .map(|(_, release)| release)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledWasmExtension {
    pub root: PathBuf,
    pub manifest: PathBuf,
    pub component: PathBuf,
    pub release: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct InstalledReleaseRecord {
    schema: u32,
    release: RegistryRelease,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustedPublisher {
    name: String,
    publisher_key: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustedPublisherCatalog {
    schema: u32,
    publishers: Vec<TrustedPublisher>,
}

impl Default for TrustedPublisherCatalog {
    fn default() -> Self {
        Self {
            schema: REGISTRY_SCHEMA,
            publishers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledWasmExtensionStatus {
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub manifest_fingerprint: String,
    pub component_blake3: String,
    pub problem: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActiveWasmExtensionLoadReport {
    pub extensions: Vec<(PluginManifest, Vec<u8>)>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WasmExtensionActivation {
    pub name: String,
    pub version: String,
    pub publisher_key_fingerprint: String,
    pub manifest_fingerprint: String,
    pub component_blake3: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WasmActivationCatalog {
    pub schema: u32,
    pub extensions: Vec<WasmExtensionActivation>,
}

impl Default for WasmActivationCatalog {
    fn default() -> Self {
        Self {
            schema: REGISTRY_SCHEMA,
            extensions: Vec::new(),
        }
    }
}

/// Loads the explicit activation ledger and the exact bytes it pins.
///
/// # Errors
/// Returns an error when the ledger or any pinned installed artifact changed.
pub fn load_active_wasm_extensions(
    root: &Path,
) -> Result<Vec<(PluginManifest, Vec<u8>)>, RegistryError> {
    let catalog = read_activation_catalog(root)?;
    let mut loaded = Vec::with_capacity(catalog.extensions.len());
    let mut aggregate_bytes = 0usize;
    for activation in catalog.extensions {
        let (manifest, component) = load_activated_extension(root, &activation)?;
        aggregate_bytes = aggregate_bytes.saturating_add(component.len());
        if aggregate_bytes > MAX_ENABLED_COMPONENT_BYTES {
            return Err(RegistryError::EnabledComponentBudgetExceeded);
        }
        loaded.push((manifest, component));
    }
    Ok(loaded)
}

/// Loads each enabled extension independently. Invalid/tampered extensions are
/// skipped while a sanitized warning is returned for the recovery surface.
///
/// # Errors
/// Returns an error when the activation ledger itself cannot be read safely.
pub fn load_active_wasm_extensions_report(
    root: &Path,
) -> Result<ActiveWasmExtensionLoadReport, RegistryError> {
    let catalog = read_activation_catalog(root)?;
    let mut report = ActiveWasmExtensionLoadReport::default();
    let mut aggregate_bytes = 0usize;
    for activation in catalog.extensions {
        match load_activated_extension(root, &activation) {
            Ok((manifest, component))
                if aggregate_bytes.saturating_add(component.len())
                    <= MAX_ENABLED_COMPONENT_BYTES =>
            {
                aggregate_bytes = aggregate_bytes.saturating_add(component.len());
                report.extensions.push((manifest, component));
            }
            Ok(_) => report.warnings.push(format!(
                "extension {} {} was skipped because the enabled component budget was exhausted",
                activation.name, activation.version
            )),
            Err(_) => report.warnings.push(format!(
                "extension {} {} was skipped because its installed trust record or artifact is invalid",
                activation.name, activation.version
            )),
        }
    }
    Ok(report)
}

fn load_activated_extension(
    root: &Path,
    activation: &WasmExtensionActivation,
) -> Result<(PluginManifest, Vec<u8>), RegistryError> {
    let (record, manifest, component) =
        read_installed_release(root, &activation.name, &activation.version).map_err(|_| {
            RegistryError::ActivationChanged {
                name: activation.name.clone(),
            }
        })?;
    let manifest_fingerprint =
        manifest
            .fingerprint()
            .map_err(|error| RegistryError::InvalidManifest {
                message: error.to_string(),
            })?;
    let component_blake3 = blake3::hash(&component).to_hex().to_string();
    let publisher_key_fingerprint =
        trusted_publisher_fingerprint(root, &activation.name).map_err(|_| {
            RegistryError::ActivationChanged {
                name: activation.name.clone(),
            }
        })?;
    if record.release.name != activation.name
        || record.release.version != activation.version
        || publisher_key_fingerprint != activation.publisher_key_fingerprint
        || manifest_fingerprint != activation.manifest_fingerprint
        || component_blake3 != activation.component_blake3
    {
        return Err(RegistryError::ActivationChanged {
            name: activation.name.clone(),
        });
    }
    Ok((manifest, component))
}

/// Explicitly pins one already-installed version. Calling this is the separate
/// activation action; installation alone never executes code.
///
/// # Errors
/// Returns an error for unsafe paths, invalid manifests, or persistence failures.
pub fn activate_installed_wasm_extension(
    root: &Path,
    name: &str,
    version: &str,
) -> Result<WasmExtensionActivation, RegistryError> {
    let (_, manifest, component) = read_installed_release(root, name, version)?;
    let activation = WasmExtensionActivation {
        name: name.to_owned(),
        version: version.to_owned(),
        publisher_key_fingerprint: trusted_publisher_fingerprint(root, name)?,
        manifest_fingerprint: manifest.fingerprint().map_err(|error| {
            RegistryError::InvalidManifest {
                message: error.to_string(),
            }
        })?,
        component_blake3: blake3::hash(&component).to_hex().to_string(),
    };
    let mut catalog = read_activation_catalog(root)?;
    catalog.extensions.retain(|entry| entry.name != name);
    if catalog.extensions.len() >= MAX_ENABLED_EXTENSIONS {
        return Err(RegistryError::TooManyEnabledExtensions);
    }
    let existing_bytes = catalog.extensions.iter().try_fold(0usize, |total, entry| {
        let (_, _, bytes) = read_installed_release(root, &entry.name, &entry.version)?;
        Ok::<_, RegistryError>(total.saturating_add(bytes.len()))
    })?;
    if existing_bytes.saturating_add(component.len()) > MAX_ENABLED_COMPONENT_BYTES {
        return Err(RegistryError::EnabledComponentBudgetExceeded);
    }
    catalog.extensions.push(activation.clone());
    catalog
        .extensions
        .sort_by(|left, right| left.name.cmp(&right.name));
    write_activation_catalog(root, &catalog)?;
    Ok(activation)
}

/// Removes an activation while preserving installed versions.
///
/// # Errors
/// Returns an error when the ledger is invalid or cannot be persisted.
pub fn deactivate_wasm_extension(root: &Path, name: &str) -> Result<bool, RegistryError> {
    let mut catalog = read_activation_catalog(root)?;
    let original = catalog.extensions.len();
    catalog.extensions.retain(|entry| entry.name != name);
    if catalog.extensions.len() == original {
        return Ok(false);
    }
    write_activation_catalog(root, &catalog)?;
    Ok(true)
}

/// Reads the bounded activation ledger.
///
/// # Errors
/// Returns an error for unsafe paths, malformed data, or invalid entries.
pub fn read_activation_catalog(root: &Path) -> Result<WasmActivationCatalog, RegistryError> {
    ensure_directory(root)?;
    let path = root.join(ACTIVATION_FILE);
    if !path.exists() {
        return Ok(WasmActivationCatalog::default());
    }
    let bytes = bounded_read(&path, 256 * 1024)?;
    let catalog: WasmActivationCatalog =
        serde_json::from_slice(&bytes).map_err(|error| RegistryError::Malformed {
            message: error.to_string(),
        })?;
    if catalog.schema != REGISTRY_SCHEMA || catalog.extensions.len() > MAX_ENABLED_EXTENSIONS {
        return Err(RegistryError::InvalidActivationCatalog);
    }
    let mut names = BTreeSet::new();
    if catalog.extensions.iter().any(|entry| {
        !is_canonical_plugin_name(&entry.name)
            || !names.insert(entry.name.as_str())
            || Version::parse(&entry.version).is_err()
            || !is_lowercase_digest(&entry.publisher_key_fingerprint)
            || !is_lowercase_digest(&entry.manifest_fingerprint)
            || !is_lowercase_digest(&entry.component_blake3)
    }) {
        return Err(RegistryError::InvalidActivationCatalog);
    }
    Ok(catalog)
}

/// Installs already-verified bytes beneath `root/name/version` without
/// following or replacing symlinks.
///
/// # Errors
/// Returns an error when verification, path validation, or atomic persistence fails.
pub fn install_verified_component(
    root: &Path,
    release: &RegistryRelease,
    trusted_publisher_key: &[u8; 32],
    component_bytes: &[u8],
) -> Result<InstalledWasmExtension, RegistryError> {
    release.verify(trusted_publisher_key)?;
    release.verify_component(component_bytes)?;
    ensure_directory(root)?;
    let canonical_root = fs::canonicalize(root).map_err(RegistryError::Io)?;
    pin_trusted_publisher(&canonical_root, &release.name, trusted_publisher_key)?;
    let name_root = canonical_root.join(&release.name);
    ensure_directory(&name_root)?;
    let version_root = name_root.join(&release.version);
    if version_root.exists() {
        let (record, manifest, component) =
            read_installed_release(&canonical_root, &release.name, &release.version)?;
        if record.release != *release
            || manifest != release.manifest
            || component != component_bytes
        {
            return Err(RegistryError::ExistingReleaseChanged);
        }
        return Ok(InstalledWasmExtension {
            root: version_root.clone(),
            manifest: version_root.join("manifest.json"),
            component: version_root.join("component.wasm"),
            release: version_root.join(RELEASE_FILE),
        });
    }
    let nonce = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let staging = name_root.join(format!(
        ".{}.{}.{}.installing",
        release.version,
        std::process::id(),
        nonce
    ));
    create_private_directory(&staging)?;
    let manifest_path = staging.join("manifest.json");
    let component_path = staging.join("component.wasm");
    let release_path = staging.join(RELEASE_FILE);
    let manifest =
        serde_json::to_vec_pretty(&release.manifest).map_err(|error| RegistryError::Malformed {
            message: error.to_string(),
        })?;
    let record = serde_json::to_vec_pretty(&InstalledReleaseRecord {
        schema: REGISTRY_SCHEMA,
        release: release.clone(),
    })
    .map_err(|error| RegistryError::Malformed {
        message: error.to_string(),
    })?;
    let publish = (|| {
        write_new_regular(&manifest_path, &manifest)?;
        write_new_regular(&component_path, component_bytes)?;
        write_new_regular(&release_path, &record)?;
        sync_directory(&staging)?;
        fs::rename(&staging, &version_root)?;
        sync_directory(&name_root)?;
        Ok::<_, std::io::Error>(())
    })();
    if publish.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    publish.map_err(RegistryError::Io)?;
    Ok(InstalledWasmExtension {
        root: version_root.clone(),
        manifest: version_root.join("manifest.json"),
        component: version_root.join("component.wasm"),
        release: version_root.join(RELEASE_FILE),
    })
}

fn create_private_directory(path: &Path) -> Result<(), RegistryError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path).map_err(RegistryError::Io)
}

fn write_new_regular(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn ensure_directory(path: &Path) -> Result<(), RegistryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(RegistryError::UnsafeInstallPath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(RegistryError::Io)
        }
        Err(error) => Err(RegistryError::Io(error)),
    }
}

fn safe_installed_version_root(
    root: &Path,
    name: &str,
    version: &str,
) -> Result<PathBuf, RegistryError> {
    Version::parse(version).map_err(|_| RegistryError::InvalidVersion)?;
    if !is_canonical_plugin_name(name) {
        return Err(RegistryError::IdentityMismatch);
    }
    let canonical_root = fs::canonicalize(root).map_err(RegistryError::Io)?;
    let name_root = canonical_root.join(name);
    if fs::symlink_metadata(&name_root)
        .is_ok_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(RegistryError::UnsafeInstallPath);
    }
    let version_root = fs::canonicalize(name_root.join(version)).map_err(RegistryError::Io)?;
    if !version_root.starts_with(&canonical_root)
        || fs::symlink_metadata(&version_root)
            .is_ok_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(RegistryError::UnsafeInstallPath);
    }
    Ok(version_root)
}

fn read_installed_release(
    root: &Path,
    name: &str,
    version: &str,
) -> Result<(InstalledReleaseRecord, PluginManifest, Vec<u8>), RegistryError> {
    let version_root = safe_installed_version_root(root, name, version)?;
    let record_bytes =
        read_installed_file(root, name, version, RELEASE_FILE, MAX_RELEASE_RECORD_BYTES)?;
    let record: InstalledReleaseRecord =
        serde_json::from_slice(&record_bytes).map_err(|error| RegistryError::Malformed {
            message: error.to_string(),
        })?;
    if record.schema != REGISTRY_SCHEMA {
        return Err(RegistryError::UnsupportedSchema {
            schema: record.schema,
        });
    }
    let trusted = trusted_publisher_key(root, name)?;
    record.release.verify(&trusted)?;
    if record.release.name != name || record.release.version != version {
        return Err(RegistryError::IdentityMismatch);
    }
    let manifest_bytes = read_installed_file(root, name, version, "manifest.json", 256 * 1024)?;
    let manifest = PluginManifest::from_slice(&manifest_bytes).map_err(|error| {
        RegistryError::InvalidManifest {
            message: error.to_string(),
        }
    })?;
    if manifest != record.release.manifest {
        return Err(RegistryError::ExistingReleaseChanged);
    }
    let component =
        read_installed_file(root, name, version, "component.wasm", MAX_COMPONENT_BYTES)?;
    record.release.verify_component(&component)?;
    let canonical = fs::canonicalize(&version_root).map_err(RegistryError::Io)?;
    if canonical != version_root {
        return Err(RegistryError::UnsafeInstallPath);
    }
    Ok((record, manifest, component))
}

/// Returns the exact verified manifest that activation would approve.
///
/// # Errors
/// Returns an error when the path, signed release record, manifest, or component is invalid.
pub fn inspect_installed_wasm_extension(
    root: &Path,
    name: &str,
    version: &str,
) -> Result<PluginManifest, RegistryError> {
    read_installed_release(root, name, version).map(|(_, manifest, _)| manifest)
}

/// Returns the exact signature-verified manifest and component bytes for
/// out-of-process validation before activation.
///
/// # Errors
/// Returns an error when the installed release or its trust record changed.
pub fn load_installed_wasm_extension(
    root: &Path,
    name: &str,
    version: &str,
) -> Result<(PluginManifest, Vec<u8>), RegistryError> {
    read_installed_release(root, name, version)
        .map(|(_, manifest, component)| (manifest, component))
}

/// Lists verified local installations and whether each exact version is active.
///
/// # Errors
/// Returns an error when the extension root or activation ledger cannot be read safely.
pub fn list_installed_wasm_extensions(
    root: &Path,
) -> Result<Vec<InstalledWasmExtensionStatus>, RegistryError> {
    ensure_directory(root)?;
    let (active, activation_problem) = match read_activation_catalog(root) {
        Ok(active) => (active, None),
        Err(_) => (
            WasmActivationCatalog::default(),
            Some("activation ledger is invalid; no WASM extensions will load".to_owned()),
        ),
    };
    let mut installed = Vec::new();
    let mut seen = BTreeSet::new();
    for name_entry in fs::read_dir(root).map_err(RegistryError::Io)? {
        let name_entry = name_entry.map_err(RegistryError::Io)?;
        let name = name_entry.file_name().to_string_lossy().into_owned();
        let metadata = name_entry.file_type().map_err(RegistryError::Io)?;
        if !metadata.is_dir() || name.starts_with('.') {
            continue;
        }
        for version_entry in fs::read_dir(name_entry.path()).map_err(RegistryError::Io)? {
            let version_entry = version_entry.map_err(RegistryError::Io)?;
            if !version_entry
                .file_type()
                .map_err(RegistryError::Io)?
                .is_dir()
            {
                continue;
            }
            let version = version_entry.file_name().to_string_lossy().into_owned();
            let enabled = active
                .extensions
                .iter()
                .any(|entry| entry.name == name && entry.version == version);
            seen.insert((name.clone(), version.clone()));
            match read_installed_release(root, &name, &version) {
                Ok((_, manifest, component)) => installed.push(InstalledWasmExtensionStatus {
                    enabled,
                    name: name.clone(),
                    version,
                    manifest_fingerprint: manifest.fingerprint().map_err(|error| {
                        RegistryError::InvalidManifest {
                            message: error.to_string(),
                        }
                    })?,
                    component_blake3: blake3::hash(&component).to_hex().to_string(),
                    problem: None,
                }),
                Err(_) => installed.push(InstalledWasmExtensionStatus {
                    enabled,
                    name: name.clone(),
                    version,
                    manifest_fingerprint: String::new(),
                    component_blake3: String::new(),
                    problem: Some(
                        "installed trust record, manifest, or component is invalid".to_owned(),
                    ),
                }),
            }
        }
    }
    for activation in active.extensions {
        if seen.insert((activation.name.clone(), activation.version.clone())) {
            installed.push(InstalledWasmExtensionStatus {
                name: activation.name,
                version: activation.version,
                enabled: true,
                manifest_fingerprint: activation.manifest_fingerprint,
                component_blake3: activation.component_blake3,
                problem: Some("enabled release is missing from the extension store".to_owned()),
            });
        }
    }
    if let Some(problem) = activation_problem {
        installed.push(InstalledWasmExtensionStatus {
            name: "activation-ledger".to_owned(),
            version: "-".to_owned(),
            enabled: false,
            manifest_fingerprint: String::new(),
            component_blake3: String::new(),
            problem: Some(problem),
        });
    }
    installed.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.version.cmp(&right.version))
    });
    Ok(installed)
}

#[cfg(unix)]
fn read_installed_file(
    root: &Path,
    name: &str,
    version: &str,
    file_name: &str,
    limit: usize,
) -> Result<Vec<u8>, RegistryError> {
    use rustix::fs::{Mode, OFlags};
    let root_fd = rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| RegistryError::Io(error.into()))?;
    let name_fd = rustix::fs::openat(
        &root_fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| RegistryError::Io(error.into()))?;
    let version_fd = rustix::fs::openat(
        &name_fd,
        version,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| RegistryError::Io(error.into()))?;
    let descriptor = rustix::fs::openat(
        &version_fd,
        file_name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| RegistryError::Io(error.into()))?;
    bounded_read_descriptor(descriptor, limit)
}

#[cfg(not(unix))]
fn read_installed_file(
    root: &Path,
    name: &str,
    version: &str,
    file_name: &str,
    limit: usize,
) -> Result<Vec<u8>, RegistryError> {
    let version_root = safe_installed_version_root(root, name, version)?;
    bounded_read(&version_root.join(file_name), limit)
}

fn bounded_read(path: &Path, limit: usize) -> Result<Vec<u8>, RegistryError> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};
        let descriptor = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| RegistryError::Io(error.into()))?;
        bounded_read_descriptor(descriptor, limit)
    }
    #[cfg(not(unix))]
    {
        let mut file = {
            let file = fs::OpenOptions::new()
                .read(true)
                .open(path)
                .map_err(RegistryError::Io)?;
            let metadata = file.metadata().map_err(RegistryError::Io)?;
            if !metadata.is_file()
                || usize::try_from(metadata.len()).map_or(true, |length| length > limit)
            {
                return Err(RegistryError::UnsafeInstallPath);
            }
            file
        };
        let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
        Read::by_ref(&mut file)
            .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(RegistryError::Io)?;
        if bytes.len() > limit {
            return Err(RegistryError::UnsafeInstallPath);
        }
        Ok(bytes)
    }
}

#[cfg(unix)]
fn bounded_read_descriptor(
    descriptor: std::os::fd::OwnedFd,
    limit: usize,
) -> Result<Vec<u8>, RegistryError> {
    use rustix::fs::FileType;
    let stat = rustix::fs::fstat(&descriptor).map_err(|error| RegistryError::Io(error.into()))?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || usize::try_from(stat.st_size).map_or(true, |length| length > limit)
    {
        return Err(RegistryError::UnsafeInstallPath);
    }
    let mut file = fs::File::from(descriptor);
    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    Read::by_ref(&mut file)
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(RegistryError::Io)?;
    if bytes.len() > limit {
        return Err(RegistryError::UnsafeInstallPath);
    }
    Ok(bytes)
}

fn write_activation_catalog(
    root: &Path,
    catalog: &WasmActivationCatalog,
) -> Result<(), RegistryError> {
    let bytes = serde_json::to_vec_pretty(catalog).map_err(|error| RegistryError::Malformed {
        message: error.to_string(),
    })?;
    atomic_replace_regular(&root.join(ACTIVATION_FILE), &bytes)
}

fn pin_trusted_publisher(
    root: &Path,
    name: &str,
    publisher_key: &[u8; 32],
) -> Result<(), RegistryError> {
    let mut catalog = read_trusted_publishers(root)?;
    let encoded = STANDARD_NO_PAD.encode(publisher_key);
    if let Some(existing) = catalog
        .publishers
        .iter()
        .find(|publisher| publisher.name == name)
    {
        return if existing.publisher_key == encoded {
            Ok(())
        } else {
            Err(RegistryError::UntrustedPublisher)
        };
    }
    if catalog.publishers.len() >= MAX_RELEASES {
        return Err(RegistryError::TooManyReleases);
    }
    catalog.publishers.push(TrustedPublisher {
        name: name.to_owned(),
        publisher_key: encoded,
    });
    catalog
        .publishers
        .sort_by(|left, right| left.name.cmp(&right.name));
    let bytes = serde_json::to_vec_pretty(&catalog).map_err(|error| RegistryError::Malformed {
        message: error.to_string(),
    })?;
    atomic_replace_regular(&root.join(TRUSTED_PUBLISHERS_FILE), &bytes)
}

fn read_trusted_publishers(root: &Path) -> Result<TrustedPublisherCatalog, RegistryError> {
    ensure_directory(root)?;
    let path = root.join(TRUSTED_PUBLISHERS_FILE);
    if !path.exists() {
        return Ok(TrustedPublisherCatalog::default());
    }
    let bytes = bounded_read(&path, MAX_TRUST_CATALOG_BYTES)?;
    let catalog: TrustedPublisherCatalog =
        serde_json::from_slice(&bytes).map_err(|error| RegistryError::Malformed {
            message: error.to_string(),
        })?;
    if catalog.schema != REGISTRY_SCHEMA || catalog.publishers.len() > MAX_RELEASES {
        return Err(RegistryError::InvalidPublisherTrustCatalog);
    }
    let mut names = BTreeSet::new();
    if catalog.publishers.iter().any(|publisher| {
        !is_canonical_plugin_name(&publisher.name)
            || !names.insert(publisher.name.as_str())
            || decode_exact::<32>(&publisher.publisher_key).is_err()
    }) {
        return Err(RegistryError::InvalidPublisherTrustCatalog);
    }
    Ok(catalog)
}

fn trusted_publisher_key(root: &Path, name: &str) -> Result<[u8; 32], RegistryError> {
    read_trusted_publishers(root)?
        .publishers
        .into_iter()
        .find(|publisher| publisher.name == name)
        .ok_or(RegistryError::UntrustedPublisher)
        .and_then(|publisher| {
            decode_exact::<32>(&publisher.publisher_key)
                .map_err(|()| RegistryError::InvalidPublisherKey)
        })
}

fn trusted_publisher_fingerprint(root: &Path, name: &str) -> Result<String, RegistryError> {
    Ok(blake3::hash(&trusted_publisher_key(root, name)?)
        .to_hex()
        .to_string())
}

fn is_canonical_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn is_lowercase_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn atomic_replace_regular(path: &Path, bytes: &[u8]) -> Result<(), RegistryError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(RegistryError::UnsafeInstallPath);
    }
    let parent = path.parent().ok_or(RegistryError::UnsafeInstallPath)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(RegistryError::UnsafeInstallPath)?;
    let nonce = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(RegistryError::Io)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(RegistryError::Io)
}

fn decode_exact<const N: usize>(value: &str) -> Result<[u8; N], ()> {
    let decoded = STANDARD_NO_PAD.decode(value).map_err(|_| ())?;
    decoded.try_into().map_err(|_| ())
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("extension registry catalog is too large")]
    CatalogTooLarge,
    #[error("extension registry catalog is malformed: {message}")]
    Malformed { message: String },
    #[error("extension registry schema {schema} is unsupported")]
    UnsupportedSchema { schema: u32 },
    #[error("extension registry contains too many releases")]
    TooManyReleases,
    #[error("duplicate extension release `{name}` version `{version}`")]
    DuplicateRelease { name: String, version: String },
    #[error("extension manifest is invalid: {message}")]
    InvalidManifest { message: String },
    #[error("extension component is invalid: {message}")]
    InvalidComponent { message: String },
    #[error("release identity does not match its manifest")]
    IdentityMismatch,
    #[error("release version is not semantic versioning")]
    InvalidVersion,
    #[error("component URL must be an authenticated HTTPS URL")]
    InvalidUrl,
    #[error("component size is invalid")]
    InvalidArtifactSize,
    #[error("component digest is invalid")]
    InvalidDigest,
    #[error("publisher key is invalid")]
    InvalidPublisherKey,
    #[error("publisher key is not trusted")]
    UntrustedPublisher,
    #[error("release signature is invalid")]
    InvalidSignature,
    #[error("downloaded component size does not match the signed release")]
    ArtifactSizeMismatch,
    #[error("downloaded component digest does not match the signed release")]
    ArtifactDigestMismatch,
    #[error("extension installation path is unsafe")]
    UnsafeInstallPath,
    #[error("extension installation escaped its root")]
    InstallEscapedRoot,
    #[error("extension activation catalog is invalid")]
    InvalidActivationCatalog,
    #[error("extension publisher trust catalog is invalid")]
    InvalidPublisherTrustCatalog,
    #[error("enabled extension `{name}` changed after approval")]
    ActivationChanged { name: String },
    #[error("installed extension release changed after signature verification")]
    ExistingReleaseChanged,
    #[error("too many WASM extensions are enabled")]
    TooManyEnabledExtensions,
    #[error("enabled WASM components exceed the aggregate byte budget")]
    EnabledComponentBudgetExceeded,
    #[error("extension installation failed: {0}")]
    Io(std::io::Error),
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;
    use crate::{PROTOCOL_VERSION, PluginCapabilities, PluginHook, PluginHookDeclaration};

    fn valid_component() -> Vec<u8> {
        let output = r#"{"directive":"continue"}"#;
        wat::parse_str(format!(
            r#"(component
              (type $hook (func (param "event" string) (param "payload-json" string) (result string)))
              (core module $module
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 4096))
                (func (export "realloc") (param i32 i32 i32 i32) (result i32)
                  (local $result i32)
                  global.get $heap
                  local.tee $result
                  local.get 3
                  i32.add
                  global.set $heap
                  local.get $result)
                (data (i32.const 256) "{}")
                (func (export "invoke") (param i32 i32 i32 i32) (result i32)
                  i32.const 128
                  i32.const 256
                  i32.store
                  i32.const 132
                  i32.const {}
                  i32.store
                  i32.const 128))
              (core instance $instance (instantiate $module))
              (func $invoke (type $hook)
                (canon lift (core func $instance "invoke")
                  (memory $instance "memory")
                  (realloc (func $instance "realloc"))))
              (export "invoke" (func $invoke)))"#,
            output.replace('"', "\\22"),
            output.len()
        ))
        .expect("component WAT")
    }

    fn signed_release_with_key(bytes: &[u8], key: [u8; 32]) -> (RegistryRelease, [u8; 32]) {
        let signing_key = SigningKey::from_bytes(&key);
        let public = signing_key.verifying_key().to_bytes();
        let mut release = RegistryRelease {
            name: "formatter".to_owned(),
            version: "1.2.3".to_owned(),
            manifest: PluginManifest {
                name: "formatter".to_owned(),
                version: "1.2.3".to_owned(),
                protocol: PROTOCOL_VERSION,
                capabilities: PluginCapabilities {
                    hooks: vec![PluginHookDeclaration::Name(PluginHook::PostTool)],
                    ..PluginCapabilities::default()
                },
            },
            component: RegistryArtifact {
                url: "https://extensions.example/formatter.wasm".to_owned(),
                blake3: blake3::hash(bytes).to_hex().to_string(),
                size: bytes.len() as u64,
            },
            publisher_key: STANDARD_NO_PAD.encode(public),
            signature: String::new(),
        };
        release.signature = STANDARD_NO_PAD.encode(
            signing_key
                .sign(&release.signing_bytes().expect("bytes"))
                .to_bytes(),
        );
        (release, public)
    }

    fn signed_release(bytes: &[u8]) -> (RegistryRelease, [u8; 32]) {
        signed_release_with_key(bytes, [7; 32])
    }

    #[test]
    fn release_signature_and_artifact_are_bound() {
        let bytes = b"component";
        let (mut release, public) = signed_release(bytes);
        release.verify(&public).expect("verified");
        release.verify_component(bytes).expect("artifact");
        release.component.url.push_str("-changed");
        assert!(matches!(
            release.verify(&public),
            Err(RegistryError::InvalidSignature)
        ));
    }

    #[test]
    fn catalog_resolution_is_semantic_not_input_order() {
        let (old, _) = signed_release(b"old");
        let mut latest = old.clone();
        latest.version = "2.0.0".to_owned();
        latest.manifest.version.clone_from(&latest.version);
        let catalog = ExtensionRegistryCatalog {
            schema: REGISTRY_SCHEMA,
            releases: vec![latest, old],
        };
        assert_eq!(
            catalog.latest("formatter").expect("latest").version,
            "2.0.0"
        );
    }

    #[test]
    fn catalog_rejects_unvalidated_terminal_text_before_listing() {
        let (mut release, _) = signed_release(b"old");
        release.name = "formatter\u{1b}[31m".to_owned();
        let bytes = serde_json::to_vec(&ExtensionRegistryCatalog {
            schema: REGISTRY_SCHEMA,
            releases: vec![release],
        })
        .expect("catalog");
        assert!(matches!(
            ExtensionRegistryCatalog::from_slice(&bytes),
            Err(RegistryError::InvalidManifest { .. } | RegistryError::IdentityMismatch)
        ));
    }

    #[test]
    fn verified_install_is_bounded_and_idempotent() {
        let bytes = b"component";
        let (release, public) = signed_release(bytes);
        let root = tempfile::tempdir().expect("root");
        let install =
            install_verified_component(root.path(), &release, &public, bytes).expect("install");
        assert_eq!(fs::read(install.component).expect("component"), bytes);
        install_verified_component(root.path(), &release, &public, bytes).expect("repeat");
        assert!(install_verified_component(root.path(), &release, &public, b"different").is_err());
    }

    #[test]
    fn installation_is_inert_until_explicit_exact_activation() {
        let bytes = valid_component();
        let (release, public) = signed_release(&bytes);
        let root = tempfile::tempdir().expect("root");
        install_verified_component(root.path(), &release, &public, &bytes).expect("install");
        assert!(
            load_active_wasm_extensions(root.path())
                .expect("inactive")
                .is_empty()
        );

        let activation =
            activate_installed_wasm_extension(root.path(), &release.name, &release.version)
                .expect("activate");
        assert_eq!(activation.name, release.name);
        assert_eq!(
            load_active_wasm_extensions(root.path())
                .expect("active")
                .len(),
            1
        );

        fs::write(
            root.path()
                .join(&release.name)
                .join(&release.version)
                .join("component.wasm"),
            b"changed",
        )
        .expect("tamper");
        assert!(matches!(
            load_active_wasm_extensions(root.path()),
            Err(RegistryError::ActivationChanged { .. })
        ));
        assert!(deactivate_wasm_extension(root.path(), &release.name).expect("disable"));
        assert!(!deactivate_wasm_extension(root.path(), &release.name).expect("disabled"));
    }

    #[test]
    fn activation_reverifies_the_install_time_signature_and_artifact() {
        let bytes = valid_component();
        let (release, public) = signed_release(&bytes);
        let root = tempfile::tempdir().expect("root");
        let installed =
            install_verified_component(root.path(), &release, &public, &bytes).expect("install");
        fs::write(
            &installed.component,
            valid_component().into_iter().chain([0]).collect::<Vec<_>>(),
        )
        .expect("tamper component");
        assert!(
            activate_installed_wasm_extension(root.path(), &release.name, &release.version)
                .is_err()
        );

        let attacker_component = b"attacker-controlled-component";
        let (attacker_release, _) = signed_release_with_key(attacker_component, [9; 32]);
        fs::write(&installed.component, attacker_component).expect("replace component");
        fs::write(
            &installed.manifest,
            serde_json::to_vec(&attacker_release.manifest).expect("encode attacker manifest"),
        )
        .expect("replace manifest");
        fs::write(
            &installed.release,
            serde_json::to_vec(&InstalledReleaseRecord {
                schema: REGISTRY_SCHEMA,
                release: attacker_release,
            })
            .expect("encode attacker release"),
        )
        .expect("replace release record");
        assert!(
            activate_installed_wasm_extension(root.path(), &release.name, &release.version)
                .is_err()
        );
    }

    #[test]
    fn publisher_key_is_pinned_outside_the_installed_release() {
        let bytes = b"component";
        let (release, public) = signed_release(bytes);
        let root = tempfile::tempdir().expect("root");
        install_verified_component(root.path(), &release, &public, bytes).expect("install");

        let (replacement, replacement_public) = signed_release_with_key(bytes, [9; 32]);
        assert!(matches!(
            install_verified_component(root.path(), &replacement, &replacement_public, bytes),
            Err(RegistryError::UntrustedPublisher)
        ));
        let trust: TrustedPublisherCatalog = serde_json::from_slice(
            &fs::read(root.path().join(TRUSTED_PUBLISHERS_FILE)).expect("trust catalog"),
        )
        .expect("decode trust catalog");
        assert_eq!(trust.publishers.len(), 1);
        assert_eq!(
            trust.publishers[0].publisher_key,
            STANDARD_NO_PAD.encode(public)
        );
    }

    #[test]
    fn activation_catalog_rejects_noncanonical_names_and_digests() {
        let root = tempfile::tempdir().expect("root");
        let activation = WasmExtensionActivation {
            name: "formatter\u{1b}[31m".to_owned(),
            version: "1.2.3".to_owned(),
            publisher_key_fingerprint: "a".repeat(64),
            manifest_fingerprint: "b".repeat(64),
            component_blake3: "c".repeat(64),
        };
        fs::write(
            root.path().join(ACTIVATION_FILE),
            serde_json::to_vec(&WasmActivationCatalog {
                schema: REGISTRY_SCHEMA,
                extensions: vec![activation],
            })
            .expect("encode control catalog"),
        )
        .expect("write control catalog");
        assert!(matches!(
            read_activation_catalog(root.path()),
            Err(RegistryError::InvalidActivationCatalog)
        ));

        let activation = WasmExtensionActivation {
            name: "formatter".to_owned(),
            version: "1.2.3".to_owned(),
            publisher_key_fingerprint: "A".repeat(64),
            manifest_fingerprint: "b".repeat(64),
            component_blake3: "c".repeat(64),
        };
        fs::write(
            root.path().join(ACTIVATION_FILE),
            serde_json::to_vec(&WasmActivationCatalog {
                schema: REGISTRY_SCHEMA,
                extensions: vec![activation],
            })
            .expect("encode uppercase catalog"),
        )
        .expect("write uppercase catalog");
        assert!(matches!(
            read_activation_catalog(root.path()),
            Err(RegistryError::InvalidActivationCatalog)
        ));
    }

    #[test]
    fn installed_status_distinguishes_inactive_and_enabled_versions() {
        let bytes = valid_component();
        let (release, public) = signed_release(&bytes);
        let root = tempfile::tempdir().expect("root");
        install_verified_component(root.path(), &release, &public, &bytes).expect("install");
        let inactive = list_installed_wasm_extensions(root.path()).expect("inactive status");
        assert_eq!(inactive.len(), 1);
        assert!(!inactive[0].enabled);
        activate_installed_wasm_extension(root.path(), &release.name, &release.version)
            .expect("activate");
        let enabled = list_installed_wasm_extensions(root.path()).expect("enabled status");
        assert!(enabled[0].enabled);
    }

    #[test]
    fn installed_status_keeps_tampered_and_invalid_activation_records_visible() {
        let bytes = valid_component();
        let (release, public) = signed_release(&bytes);
        let root = tempfile::tempdir().expect("root");
        let installed =
            install_verified_component(root.path(), &release, &public, &bytes).expect("install");
        activate_installed_wasm_extension(root.path(), &release.name, &release.version)
            .expect("activate");
        fs::write(&installed.component, b"tampered").expect("tamper");
        let status = list_installed_wasm_extensions(root.path()).expect("status");
        assert_eq!(status.len(), 1);
        assert!(status[0].enabled);
        assert!(status[0].problem.is_some());

        fs::write(root.path().join(ACTIVATION_FILE), b"not json").expect("bad ledger");
        let status = list_installed_wasm_extensions(root.path()).expect("recovering status");
        assert!(status.iter().any(|entry| {
            entry.name == "activation-ledger"
                && entry
                    .problem
                    .as_deref()
                    .is_some_and(|problem| problem.contains("invalid"))
        }));
        assert!(status.iter().any(|entry| entry.name == release.name));
    }
}
