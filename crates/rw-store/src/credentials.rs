//! Provider-blind credential lookup and storage.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(target_os = "macos")]
use std::sync::Condvar;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Service name of the single OS-keychain vault used by Rottweiler.
pub const KEYCHAIN_VAULT_SERVICE: &str = "dev.rottweiler.credential-vault";
/// Account/identifier of the single OS-keychain vault used by Rottweiler.
pub const KEYCHAIN_VAULT_ID: &str = "credentials";
/// Set to `file` to bypass OS-keychain APIs and use the warned 0600 fallback.
pub const CREDENTIAL_BACKEND_ENV: &str = "ROTTWEILER_CREDENTIAL_BACKEND";

const LEGACY_KEYCHAIN_SERVICE: &str = "dev.rottweiler.credentials";
const KEYCHAIN_VAULT_VERSION: u8 = 1;
const CREDENTIAL_FILE_VERSION: u8 = 1;
#[cfg(target_os = "macos")]
const PASSIVE_KEYCHAIN_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A value that must not appear in diagnostics or gain serialization by accident.
///
/// The wrapped value is available only through [`Secret::expose_secret`]. `Secret`
/// intentionally does not implement `serde::Serialize`.
///
/// ```compile_fail
/// use rw_store::credentials::Secret;
///
/// let secret = Secret::new(String::from("do-not-serialize"));
/// let _encoded = toml::to_string(&secret);
/// ```
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wraps sensitive material.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Explicitly borrows the sensitive material for an authenticated boundary.
    #[must_use]
    pub const fn expose_secret(&self) -> &T {
        &self.0
    }

    /// Explicitly consumes the wrapper at an authenticated boundary.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// Provider-independent names used to locate one credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialReference {
    identifier: String,
    environment_variable: Option<String>,
}

impl CredentialReference {
    /// Creates a keychain/file reference without an environment override.
    #[must_use]
    pub fn new(identifier: impl Into<String>) -> Self {
        Self {
            identifier: identifier.into(),
            environment_variable: None,
        }
    }

    /// Makes an environment variable the highest-precedence source.
    #[must_use]
    pub fn with_environment(mut self, variable: impl Into<String>) -> Self {
        self.environment_variable = Some(variable.into());
        self
    }

    /// Stable identifier used by the keychain and fallback file.
    #[must_use]
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// Optional highest-precedence environment variable.
    #[must_use]
    pub fn environment_variable(&self) -> Option<&str> {
        self.environment_variable.as_deref()
    }

    fn validate(&self) -> Result<(), CredentialError> {
        if self.identifier.trim().is_empty() {
            return Err(CredentialError::InvalidReference);
        }
        if self
            .environment_variable
            .as_deref()
            .is_some_and(|variable| variable.trim().is_empty())
        {
            return Err(CredentialError::InvalidEnvironmentReference);
        }
        Ok(())
    }
}

/// The source from which a credential was resolved or stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// A process environment variable.
    Environment(String),
    /// The operating system's secure credential store.
    OsKeychain,
    /// The explicitly warned plaintext fallback.
    FallbackFile(PathBuf),
}

/// Security warnings that callers must surface to the user.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CredentialWarning {
    /// The OS keychain was not used and a local plaintext file supplied the value.
    #[error(
        "credential is using plaintext fallback file {path}; access is restricted to mode 0600"
    )]
    PlaintextFileFallback {
        /// Path whose use must be shown to the user.
        path: PathBuf,
    },
}

/// A resolved credential and its audit metadata.
#[derive(Debug)]
pub struct ResolvedCredential {
    secret: Secret<String>,
    source: CredentialSource,
    warnings: Vec<CredentialWarning>,
}

/// One secret-safe result from a non-mutating batch credential inventory.
#[derive(Debug)]
pub enum CredentialInventoryItem {
    /// A value exists; callers may use it only at an authenticated boundary.
    Present(ResolvedCredential),
    /// No configured source contains the reference.
    Missing,
    /// The keychain was unavailable and no fallback value exists.
    StoreUnavailable,
}

enum EnvironmentInventoryValue {
    NotConfigured,
    Missing,
    Present(String),
}

fn environment_only_inventory(
    references: &[CredentialReference],
    environment_values: &[EnvironmentInventoryValue],
) -> Option<Vec<CredentialInventoryItem>> {
    let values = environment_values
        .iter()
        .map(|environment| match environment {
            EnvironmentInventoryValue::Present(value) => Some(value),
            EnvironmentInventoryValue::NotConfigured | EnvironmentInventoryValue::Missing => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(
        references
            .iter()
            .zip(values)
            .map(|(reference, value)| {
                CredentialInventoryItem::Present(ResolvedCredential {
                    secret: Secret::new(value.clone()),
                    source: CredentialSource::Environment(
                        reference
                            .environment_variable()
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    warnings: Vec::new(),
                })
            })
            .collect(),
    )
}

impl ResolvedCredential {
    /// Sensitive value, exposed only through an explicit method call.
    #[must_use]
    pub const fn secret(&self) -> &Secret<String> {
        &self.secret
    }

    /// Winning source after applying environment, keychain, then file precedence.
    #[must_use]
    pub const fn source(&self) -> &CredentialSource {
        &self.source
    }

    /// Warnings that must be surfaced by the calling UI.
    #[must_use]
    pub fn warnings(&self) -> &[CredentialWarning] {
        &self.warnings
    }
}

/// Result of persisting a credential.
#[derive(Debug)]
pub struct StoredCredential {
    source: CredentialSource,
    warnings: Vec<CredentialWarning>,
}

impl StoredCredential {
    /// Storage backend that accepted the value.
    #[must_use]
    pub const fn source(&self) -> &CredentialSource {
        &self.source
    }

    /// Warnings that must be surfaced by the calling UI.
    #[must_use]
    pub fn warnings(&self) -> &[CredentialWarning] {
        &self.warnings
    }
}

/// Sanitized credential failures. No variant contains credential material.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// The reference cannot safely identify a stored value.
    #[error("credential reference must have a non-empty identifier")]
    InvalidReference,
    /// The configured environment reference is empty.
    #[error("credential environment reference must not be empty")]
    InvalidEnvironmentReference,
    /// A referenced environment value was not valid Unicode.
    #[error("credential environment variable {name} is not valid Unicode")]
    NonUnicodeEnvironment {
        /// Environment variable name (never its value).
        name: String,
    },
    /// No configured source contains the requested credential.
    #[error("credential {identifier:?} was not found")]
    NotFound {
        /// Non-secret reference identifier.
        identifier: String,
    },
    /// The keychain could not be accessed and there was no file fallback.
    #[error("OS keychain is unavailable for credential {identifier:?}")]
    KeychainUnavailable {
        /// Non-secret reference identifier.
        identifier: String,
    },
    /// The single keychain vault could not be decoded safely.
    #[error("OS keychain credential vault is malformed")]
    MalformedKeychainVault,
    /// The single keychain vault could not be encoded safely.
    #[error("could not encode OS keychain credential vault")]
    EncodeKeychainVault,
    /// A fallback file had unsafe group/other permissions.
    #[error("credential fallback file {path} has insecure permissions {mode:#o}; expected 0600")]
    InsecurePermissions {
        /// Insecure file.
        path: PathBuf,
        /// Observed Unix permission bits.
        mode: u32,
    },
    /// A fallback file could not be read.
    #[error("could not read credential fallback file {path}: {source}")]
    ReadFile {
        /// File path.
        path: PathBuf,
        /// Underlying I/O error, which contains no file contents.
        #[source]
        source: std::io::Error,
    },
    /// A fallback file was malformed. The parser source is suppressed to prevent excerpts.
    #[error("credential fallback file {path} is malformed")]
    MalformedFile {
        /// File path.
        path: PathBuf,
    },
    /// A fallback path was not a regular file (for example, it was a symlink).
    #[error("credential fallback path {path} is not a regular file")]
    UnsafeFileType {
        /// Unsafe path.
        path: PathBuf,
    },
    /// A fallback file could not be securely written.
    #[error("could not write credential fallback file {path}: {source}")]
    WriteFile {
        /// File path.
        path: PathBuf,
        /// Underlying I/O error, which contains no credential contents.
        #[source]
        source: std::io::Error,
    },
    /// The in-memory fallback document could not be encoded.
    #[error("could not encode credential fallback file {path}")]
    EncodeFile {
        /// File path.
        path: PathBuf,
    },
}

/// Injectable process-environment boundary.
pub trait CredentialEnvironment {
    /// Looks up a value without ever logging it.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when the process value cannot be represented safely.
    fn get(&self, name: &str) -> Result<Option<String>, CredentialError>;
}

/// Real process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnvironment;

impl CredentialEnvironment for SystemEnvironment {
    fn get(&self, name: &str) -> Result<Option<String>, CredentialError> {
        match env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => Err(CredentialError::NonUnicodeEnvironment {
                name: name.to_owned(),
            }),
        }
    }
}

/// Sanitized keychain outcome used by injected test backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("OS keychain is unavailable")]
pub struct KeychainUnavailable;

/// Injectable secure-credential-store boundary.
pub trait CredentialKeychain {
    /// Reads one keychain item, returning `None` when no entry exists.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainUnavailable`] without exposing backend diagnostics.
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable>;

    /// Reads one keychain item for an explicit, user-initiated operation.
    ///
    /// Passive inventory may impose a short deadline so that a status screen
    /// never hangs on an authorization dialog. Active provider composition uses
    /// this boundary instead and may wait for the user to authorize the vault.
    /// Test backends that do not distinguish those modes may use the default.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainUnavailable`] without exposing backend diagnostics.
    fn get_authorized(
        &self,
        identifier: &str,
    ) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        self.get(identifier)
    }

    /// Creates or replaces a credential.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainUnavailable`] without exposing backend diagnostics.
    fn set(&self, identifier: &str, secret: &Secret<String>) -> Result<(), KeychainUnavailable>;

    /// Reads the current vault value without using a process cache.
    ///
    /// Whole-vault mutations use this only while holding the cross-process vault
    /// lock, so they merge against the latest durable map without adding reads to
    /// ordinary credential resolution.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainUnavailable`] without exposing backend diagnostics.
    fn get_fresh(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        self.get(identifier)
    }

    /// Reads a pre-vault credential for one-time migration.
    ///
    /// Backends that did not distinguish the legacy service may use the default
    /// implementation. Production overrides it so new vault data and old entries
    /// cannot collide.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainUnavailable`] without exposing backend diagnostics.
    fn get_legacy(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        self.get(identifier)
    }

    /// Performs an explicitly user-authorized legacy read during one-time
    /// migration. Backends without a passive/authorized distinction delegate
    /// to [`Self::get_legacy`].
    ///
    /// # Errors
    ///
    /// Returns [`KeychainUnavailable`] without exposing backend diagnostics.
    fn get_legacy_authorized(
        &self,
        identifier: &str,
    ) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        self.get_legacy(identifier)
    }
}

/// Operating-system keychain backed by the current `keyring` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeychain;

enum ProcessVaultCache {
    Unloaded,
    #[cfg(target_os = "macos")]
    Reading(Arc<ProcessVaultRead>),
    Loaded(Option<String>),
    Unavailable,
}

#[cfg(target_os = "macos")]
struct ProcessVaultRead {
    result: Mutex<Option<Result<Option<String>, KeychainUnavailable>>>,
    ready: Condvar,
}

#[cfg(target_os = "macos")]
impl ProcessVaultRead {
    fn start<F>(read: F) -> Result<Arc<Self>, KeychainUnavailable>
    where
        F: FnOnce() -> Result<Option<Secret<String>>, KeychainUnavailable> + Send + 'static,
    {
        let operation = Arc::new(Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        });
        let background = Arc::clone(&operation);
        std::thread::Builder::new()
            .name("rottweiler-keychain-read".to_owned())
            .spawn(move || {
                let result = read().map(|value| value.map(Secret::into_inner));
                if let Ok(mut slot) = background.result.lock() {
                    *slot = Some(result);
                    background.ready.notify_all();
                }
            })
            .map_err(|_| KeychainUnavailable)?;
        Ok(operation)
    }

    fn wait(
        &self,
        timeout: Option<std::time::Duration>,
    ) -> Result<Option<String>, KeychainUnavailable> {
        let guard = self.result.lock().map_err(|_| KeychainUnavailable)?;
        let guard = if let Some(timeout) = timeout {
            self.ready
                .wait_timeout_while(guard, timeout, |result| result.is_none())
                .map_err(|_| KeychainUnavailable)?
                .0
        } else {
            self.ready
                .wait_while(guard, |result| result.is_none())
                .map_err(|_| KeychainUnavailable)?
        };
        guard.clone().unwrap_or(Err(KeychainUnavailable))
    }
}

#[cfg(target_os = "macos")]
fn read_process_vault_explicit<F>(
    cache: &Mutex<ProcessVaultCache>,
    reuse_loaded: bool,
    read: F,
) -> Result<Option<Secret<String>>, KeychainUnavailable>
where
    F: FnOnce() -> Result<Option<Secret<String>>, KeychainUnavailable> + Send + 'static,
{
    let operation = {
        let mut cache = cache.lock().map_err(|_| KeychainUnavailable)?;
        match &*cache {
            ProcessVaultCache::Loaded(value) if reuse_loaded => {
                return Ok(value.clone().map(Secret::new));
            }
            ProcessVaultCache::Reading(operation) => Arc::clone(operation),
            ProcessVaultCache::Unloaded
            | ProcessVaultCache::Unavailable
            | ProcessVaultCache::Loaded(_) => {
                let operation = ProcessVaultRead::start(read)?;
                *cache = ProcessVaultCache::Reading(Arc::clone(&operation));
                operation
            }
        }
    };
    let result = operation.wait(None);
    let mut cache = cache.lock().map_err(|_| KeychainUnavailable)?;
    if matches!(&*cache, ProcessVaultCache::Reading(active) if Arc::ptr_eq(active, &operation)) {
        *cache = match &result {
            Ok(value) => ProcessVaultCache::Loaded(value.clone()),
            Err(_) => ProcessVaultCache::Unavailable,
        };
    }
    result.map(|value| value.map(Secret::new))
}

fn process_vault_cache() -> &'static Mutex<ProcessVaultCache> {
    static CACHE: OnceLock<Mutex<ProcessVaultCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ProcessVaultCache::Unloaded))
}

fn read_keychain_item(
    service: &str,
    identifier: &str,
) -> Result<Option<Secret<String>>, KeychainUnavailable> {
    #[cfg(target_os = "macos")]
    {
        let service = service.to_owned();
        let identifier = identifier.to_owned();
        bounded_keychain_read(PASSIVE_KEYCHAIN_READ_TIMEOUT, move || {
            read_keychain_item_blocking(&service, &identifier)
        })
    }

    #[cfg(not(target_os = "macos"))]
    read_keychain_item_blocking(service, identifier)
}

fn read_keychain_item_blocking(
    service: &str,
    identifier: &str,
) -> Result<Option<Secret<String>>, KeychainUnavailable> {
    let entry = keyring::Entry::new(service, identifier).map_err(|_| KeychainUnavailable)?;
    match entry.get_password() {
        Ok(value) => Ok(Some(Secret::new(value))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(KeychainUnavailable),
    }
}

#[cfg(target_os = "macos")]
fn bounded_keychain_read<F>(
    timeout: std::time::Duration,
    read: F,
) -> Result<Option<Secret<String>>, KeychainUnavailable>
where
    F: FnOnce() -> Result<Option<Secret<String>>, KeychainUnavailable> + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rottweiler-keychain-read".to_owned())
        .spawn(move || {
            let _ = sender.send(read());
        })
        .map_err(|_| KeychainUnavailable)?;
    receiver
        .recv_timeout(timeout)
        .unwrap_or(Err(KeychainUnavailable))
}

fn write_keychain_item(
    service: &str,
    identifier: &str,
    secret: &Secret<String>,
) -> Result<(), KeychainUnavailable> {
    let entry = keyring::Entry::new(service, identifier).map_err(|_| KeychainUnavailable)?;
    entry
        .set_password(secret.expose_secret())
        .map_err(|_| KeychainUnavailable)
}

fn os_keychain_disabled() -> bool {
    keychain_backend_is_file(env::var_os(CREDENTIAL_BACKEND_ENV).as_deref())
}

fn keychain_backend_is_file(value: Option<&std::ffi::OsStr>) -> bool {
    match value {
        None => false,
        Some(value) if value == "keychain" => false,
        Some(value) if value == "file" => true,
        // Invalid and non-Unicode values fail closed without touching OS APIs.
        Some(_) => true,
    }
}

impl CredentialKeychain for OsKeychain {
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        if identifier != KEYCHAIN_VAULT_ID || os_keychain_disabled() {
            return Err(KeychainUnavailable);
        }

        #[cfg(target_os = "macos")]
        {
            let operation = {
                let mut cache = process_vault_cache()
                    .lock()
                    .map_err(|_| KeychainUnavailable)?;
                match &*cache {
                    ProcessVaultCache::Loaded(value) => {
                        return Ok(value.clone().map(Secret::new));
                    }
                    ProcessVaultCache::Unavailable => return Err(KeychainUnavailable),
                    ProcessVaultCache::Reading(operation) => Arc::clone(operation),
                    ProcessVaultCache::Unloaded => {
                        let operation = ProcessVaultRead::start(|| {
                            read_keychain_item_blocking(KEYCHAIN_VAULT_SERVICE, KEYCHAIN_VAULT_ID)
                        })?;
                        *cache = ProcessVaultCache::Reading(Arc::clone(&operation));
                        operation
                    }
                }
            };
            let result = operation.wait(Some(PASSIVE_KEYCHAIN_READ_TIMEOUT));
            if let Ok(mut cache) = process_vault_cache().lock()
                && matches!(&*cache, ProcessVaultCache::Reading(active) if Arc::ptr_eq(active, &operation))
                && let Ok(value) = &result
            {
                *cache = ProcessVaultCache::Loaded(value.clone());
            }
            result.map(|value| value.map(Secret::new))
        }

        #[cfg(not(target_os = "macos"))]
        {
            let mut cache = process_vault_cache()
                .lock()
                .map_err(|_| KeychainUnavailable)?;
            match &*cache {
                ProcessVaultCache::Loaded(value) => {
                    return Ok(value.clone().map(Secret::new));
                }
                ProcessVaultCache::Unavailable => return Err(KeychainUnavailable),
                ProcessVaultCache::Unloaded => {}
            }

            match read_keychain_item(KEYCHAIN_VAULT_SERVICE, identifier) {
                Ok(value) => {
                    *cache = ProcessVaultCache::Loaded(
                        value.as_ref().map(|secret| secret.expose_secret().clone()),
                    );
                    Ok(value)
                }
                Err(error) => {
                    *cache = ProcessVaultCache::Unavailable;
                    Err(error)
                }
            }
        }
    }

    fn get_authorized(
        &self,
        identifier: &str,
    ) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        if identifier != KEYCHAIN_VAULT_ID || os_keychain_disabled() {
            return Err(KeychainUnavailable);
        }

        #[cfg(target_os = "macos")]
        {
            read_process_vault_explicit(process_vault_cache(), true, || {
                read_keychain_item_blocking(KEYCHAIN_VAULT_SERVICE, KEYCHAIN_VAULT_ID)
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let mut cache = process_vault_cache()
                .lock()
                .map_err(|_| KeychainUnavailable)?;
            if let ProcessVaultCache::Loaded(value) = &*cache {
                return Ok(value.clone().map(Secret::new));
            }
            let value = read_keychain_item_blocking(KEYCHAIN_VAULT_SERVICE, identifier)?;
            *cache = ProcessVaultCache::Loaded(
                value.as_ref().map(|secret| secret.expose_secret().clone()),
            );
            Ok(value)
        }
    }

    fn set(&self, identifier: &str, secret: &Secret<String>) -> Result<(), KeychainUnavailable> {
        if identifier != KEYCHAIN_VAULT_ID || os_keychain_disabled() {
            return Err(KeychainUnavailable);
        }

        let mut cache = process_vault_cache()
            .lock()
            .map_err(|_| KeychainUnavailable)?;
        match write_keychain_item(KEYCHAIN_VAULT_SERVICE, identifier, secret) {
            Ok(()) => {
                *cache = ProcessVaultCache::Loaded(Some(secret.expose_secret().clone()));
                Ok(())
            }
            Err(error) => {
                *cache = ProcessVaultCache::Unavailable;
                Err(error)
            }
        }
    }

    fn get_fresh(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        if identifier != KEYCHAIN_VAULT_ID || os_keychain_disabled() {
            return Err(KeychainUnavailable);
        }

        #[cfg(target_os = "macos")]
        {
            read_process_vault_explicit(process_vault_cache(), false, || {
                read_keychain_item_blocking(KEYCHAIN_VAULT_SERVICE, KEYCHAIN_VAULT_ID)
            })
        }

        #[cfg(not(target_os = "macos"))]
        read_keychain_item_blocking(KEYCHAIN_VAULT_SERVICE, identifier)
    }

    fn get_legacy(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        if os_keychain_disabled() {
            return Err(KeychainUnavailable);
        }
        read_keychain_item(LEGACY_KEYCHAIN_SERVICE, identifier)
    }

    fn get_legacy_authorized(
        &self,
        identifier: &str,
    ) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        if os_keychain_disabled() {
            return Err(KeychainUnavailable);
        }
        read_keychain_item_blocking(LEGACY_KEYCHAIN_SERVICE, identifier)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialVault {
    version: u8,
    credentials: BTreeMap<String, String>,
}

impl Default for CredentialVault {
    fn default() -> Self {
        Self {
            version: KEYCHAIN_VAULT_VERSION,
            credentials: BTreeMap::new(),
        }
    }
}

enum CachedVault {
    Unloaded,
    Loaded(CredentialVault),
    Unavailable,
    Malformed,
}

struct CredentialVaultCache {
    vault: CachedVault,
    legacy_bootstrap_pending: bool,
    writes_unavailable: bool,
}

impl Default for CredentialVaultCache {
    fn default() -> Self {
        Self {
            vault: CachedVault::Unloaded,
            legacy_bootstrap_pending: false,
            writes_unavailable: false,
        }
    }
}

#[derive(Clone, Copy)]
enum VaultAccessError {
    Unavailable,
    Malformed,
    Encode,
}

/// Credential manager with injectable environment and keychain boundaries.
pub struct CredentialManager<E = SystemEnvironment, K = OsKeychain> {
    environment: E,
    keychain: K,
    fallback_path: PathBuf,
    vault_cache: Arc<Mutex<CredentialVaultCache>>,
}

impl<E, K> Clone for CredentialManager<E, K>
where
    E: Clone,
    K: Clone,
{
    fn clone(&self) -> Self {
        Self {
            environment: self.environment.clone(),
            keychain: self.keychain.clone(),
            fallback_path: self.fallback_path.clone(),
            vault_cache: self.vault_cache.clone(),
        }
    }
}

fn process_credential_vault_cache() -> Arc<Mutex<CredentialVaultCache>> {
    static CACHE: OnceLock<Arc<Mutex<CredentialVaultCache>>> = OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(Mutex::new(CredentialVaultCache::default())))
        .clone()
}

impl CredentialManager<SystemEnvironment, OsKeychain> {
    /// Creates the production manager using the process environment and OS keychain.
    #[must_use]
    pub fn system(fallback_path: impl Into<PathBuf>) -> Self {
        Self {
            environment: SystemEnvironment,
            keychain: OsKeychain,
            fallback_path: fallback_path.into(),
            vault_cache: process_credential_vault_cache(),
        }
    }
}

impl<E, K> CredentialManager<E, K>
where
    E: CredentialEnvironment,
    K: CredentialKeychain,
{
    /// Inventories many references with one vault read and at most one fallback
    /// document read. This path never performs legacy migration or any write.
    ///
    /// Returned entries align exactly with `references`; secret values remain
    /// inside [`ResolvedCredential`] and its redacted debug boundary.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for an invalid reference/environment value,
    /// malformed vault/fallback document, or unsafe fallback file.
    pub fn resolve_inventory(
        &self,
        references: &[CredentialReference],
    ) -> Result<Vec<CredentialInventoryItem>, CredentialError> {
        for reference in references {
            reference.validate()?;
        }
        let environment_values = references
            .iter()
            .map(|reference| {
                let Some(variable) = reference.environment_variable() else {
                    return Ok(EnvironmentInventoryValue::NotConfigured);
                };
                Ok(self
                    .environment
                    .get(variable)?
                    .filter(|value| !value.is_empty())
                    .map_or(
                        EnvironmentInventoryValue::Missing,
                        EnvironmentInventoryValue::Present,
                    ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(inventory) = environment_only_inventory(references, &environment_values) {
            return Ok(inventory);
        }
        let (vault, keychain_unavailable) = {
            let mut cache =
                self.vault_cache
                    .lock()
                    .map_err(|_| CredentialError::KeychainUnavailable {
                        identifier: "credential-vault".to_owned(),
                    })?;
            let unavailable = match self.load_vault(&mut cache) {
                Ok(()) => false,
                Err(VaultAccessError::Unavailable) => true,
                Err(VaultAccessError::Malformed) => {
                    return Err(CredentialError::MalformedKeychainVault);
                }
                Err(VaultAccessError::Encode) => {
                    return Err(CredentialError::EncodeKeychainVault);
                }
            };
            let values = match &cache.vault {
                CachedVault::Loaded(vault) => vault.credentials.clone(),
                CachedVault::Unavailable | CachedVault::Unloaded => BTreeMap::new(),
                CachedVault::Malformed => return Err(CredentialError::MalformedKeychainVault),
            };
            (values, unavailable)
        };
        let fallback = if fallback_metadata(&self.fallback_path)?.is_some() {
            Some(read_document(&self.fallback_path)?)
        } else {
            None
        };
        Ok(references
            .iter()
            .zip(environment_values)
            .map(|(reference, environment)| {
                if let EnvironmentInventoryValue::Present(value) = environment {
                    return CredentialInventoryItem::Present(ResolvedCredential {
                        secret: Secret::new(value),
                        source: CredentialSource::Environment(
                            reference
                                .environment_variable()
                                .unwrap_or_default()
                                .to_owned(),
                        ),
                        warnings: Vec::new(),
                    });
                }
                if let Some(value) = vault.get(reference.identifier()) {
                    return CredentialInventoryItem::Present(ResolvedCredential {
                        secret: Secret::new(value.clone()),
                        source: CredentialSource::OsKeychain,
                        warnings: Vec::new(),
                    });
                }
                if let Some(value) = fallback
                    .as_ref()
                    .and_then(|document| document.credentials.get(reference.identifier()))
                {
                    return CredentialInventoryItem::Present(ResolvedCredential {
                        secret: Secret::new(value.clone()),
                        source: CredentialSource::FallbackFile(self.fallback_path.clone()),
                        warnings: vec![fallback_warning(
                            &self.fallback_path,
                            reference.identifier(),
                        )],
                    });
                }
                if keychain_unavailable {
                    CredentialInventoryItem::StoreUnavailable
                } else {
                    CredentialInventoryItem::Missing
                }
            })
            .collect())
    }

    /// Creates a manager with deterministic/injectable external boundaries.
    #[must_use]
    pub fn with_backends(environment: E, keychain: K, fallback_path: impl Into<PathBuf>) -> Self {
        Self {
            environment,
            keychain,
            fallback_path: fallback_path.into(),
            vault_cache: Arc::new(Mutex::new(CredentialVaultCache::default())),
        }
    }

    /// Resolves environment first, OS keychain second, and the file fallback last.
    ///
    /// # Errors
    ///
    /// Returns a sanitized [`CredentialError`] for invalid references, unavailable
    /// sources, insecure fallback permissions, or unreadable fallback data.
    pub fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<ResolvedCredential, CredentialError> {
        self.resolve_with_mode(reference, false)
    }

    /// Resolves a credential for an explicit active operation.
    ///
    /// On macOS this path may wait for the user to authorize the single
    /// Rottweiler vault. It also retries after a prior passive inventory timed
    /// out, so a two-second status probe cannot poison the subsequent model run.
    ///
    /// # Errors
    ///
    /// Returns the same sanitized errors as [`Self::resolve`].
    pub fn resolve_authorized(
        &self,
        reference: &CredentialReference,
    ) -> Result<ResolvedCredential, CredentialError> {
        self.resolve_with_mode(reference, true)
    }

    fn resolve_with_mode(
        &self,
        reference: &CredentialReference,
        authorized: bool,
    ) -> Result<ResolvedCredential, CredentialError> {
        reference.validate()?;

        if let Some(variable) = reference.environment_variable()
            && let Some(value) = self.environment.get(variable)?
            && !value.is_empty()
        {
            return Ok(ResolvedCredential {
                secret: Secret::new(value),
                source: CredentialSource::Environment(variable.to_owned()),
                warnings: Vec::new(),
            });
        }

        let keychain = if authorized {
            self.resolve_from_vault_authorized(reference.identifier())
        } else {
            self.resolve_from_vault(reference.identifier())
        };
        let keychain_unavailable = match keychain {
            Ok(Some(secret)) => {
                return Ok(ResolvedCredential {
                    secret,
                    source: CredentialSource::OsKeychain,
                    warnings: Vec::new(),
                });
            }
            Ok(None) => false,
            Err(VaultAccessError::Unavailable) => true,
            Err(VaultAccessError::Malformed) => {
                return Err(CredentialError::MalformedKeychainVault);
            }
            Err(VaultAccessError::Encode) => {
                return Err(CredentialError::EncodeKeychainVault);
            }
        };

        if let Some(secret) = read_fallback(&self.fallback_path, reference.identifier())? {
            let warning = fallback_warning(&self.fallback_path, reference.identifier());
            return Ok(ResolvedCredential {
                secret: Secret::new(secret),
                source: CredentialSource::FallbackFile(self.fallback_path.clone()),
                warnings: vec![warning],
            });
        }

        if keychain_unavailable {
            Err(CredentialError::KeychainUnavailable {
                identifier: reference.identifier().to_owned(),
            })
        } else {
            Err(CredentialError::NotFound {
                identifier: reference.identifier().to_owned(),
            })
        }
    }

    fn resolve_from_vault_authorized(
        &self,
        identifier: &str,
    ) -> Result<Option<Secret<String>>, VaultAccessError> {
        let mut cache = self
            .vault_cache
            .lock()
            .map_err(|_| VaultAccessError::Unavailable)?;
        match &cache.vault {
            CachedVault::Loaded(vault) => {
                let value = vault.credentials.get(identifier).cloned().map(Secret::new);
                let legacy_bootstrap_pending = cache.legacy_bootstrap_pending;
                drop(cache);
                return if value.is_none() && legacy_bootstrap_pending {
                    self.resolve_from_vault_with_legacy_mode(identifier, true)
                } else {
                    Ok(value)
                };
            }
            CachedVault::Malformed => return Err(VaultAccessError::Malformed),
            CachedVault::Unloaded | CachedVault::Unavailable => {}
        }
        let encoded = self
            .keychain
            .get_authorized(KEYCHAIN_VAULT_ID)
            .map_err(|_| VaultAccessError::Unavailable)?;
        let vault_was_missing = encoded.is_none();
        let vault = match encoded {
            Some(encoded) => decode_vault(&encoded).map_err(|()| VaultAccessError::Malformed)?,
            None => CredentialVault::default(),
        };
        let value = vault.credentials.get(identifier).cloned().map(Secret::new);
        cache.vault = CachedVault::Loaded(vault);
        cache.legacy_bootstrap_pending = vault_was_missing;
        drop(cache);
        if value.is_none() && vault_was_missing {
            self.resolve_from_vault_with_legacy_mode(identifier, true)
        } else {
            Ok(value)
        }
    }

    /// Stores in the OS keychain, falling back to a mode-0600 plaintext file.
    ///
    /// # Errors
    ///
    /// Returns a sanitized [`CredentialError`] when the reference is invalid or the
    /// secure fallback file cannot be read, encoded, or written.
    pub fn store(
        &self,
        reference: &CredentialReference,
        secret: &Secret<String>,
    ) -> Result<StoredCredential, CredentialError> {
        reference.validate()?;

        match self.store_in_vault(reference.identifier(), secret) {
            Ok(()) => {
                return Ok(StoredCredential {
                    source: CredentialSource::OsKeychain,
                    warnings: Vec::new(),
                });
            }
            Err(VaultAccessError::Malformed) => {
                return Err(CredentialError::MalformedKeychainVault);
            }
            Err(VaultAccessError::Encode) => {
                return Err(CredentialError::EncodeKeychainVault);
            }
            Err(VaultAccessError::Unavailable) => {}
        }

        write_fallback(
            &self.fallback_path,
            reference.identifier(),
            secret.expose_secret(),
        )?;
        let warning = fallback_warning(&self.fallback_path, reference.identifier());
        Ok(StoredCredential {
            source: CredentialSource::FallbackFile(self.fallback_path.clone()),
            warnings: vec![warning],
        })
    }

    fn resolve_from_vault(
        &self,
        identifier: &str,
    ) -> Result<Option<Secret<String>>, VaultAccessError> {
        self.resolve_from_vault_with_legacy_mode(identifier, false)
    }

    fn resolve_from_vault_with_legacy_mode(
        &self,
        identifier: &str,
        authorized_legacy: bool,
    ) -> Result<Option<Secret<String>>, VaultAccessError> {
        let mut cache = self
            .vault_cache
            .lock()
            .map_err(|_| VaultAccessError::Unavailable)?;
        self.load_vault(&mut cache)?;

        let CachedVault::Loaded(vault) = &cache.vault else {
            return match &cache.vault {
                CachedVault::Malformed => Err(VaultAccessError::Malformed),
                CachedVault::Unavailable | CachedVault::Unloaded => {
                    Err(VaultAccessError::Unavailable)
                }
                CachedVault::Loaded(_) => unreachable!("loaded vault was matched above"),
            };
        };
        if let Some(value) = vault.credentials.get(identifier) {
            return Ok(Some(Secret::new(value.clone())));
        }

        if cache.writes_unavailable {
            return Err(VaultAccessError::Unavailable);
        }
        if !cache.legacy_bootstrap_pending {
            return Ok(None);
        }
        // Exactly one legacy identifier may be probed, and only when the new
        // vault did not exist. This bounds first-run migration to one old
        // keychain access instead of recreating a prompt per logical credential.
        cache.legacy_bootstrap_pending = false;

        let _write_lock = match acquire_vault_write_lock() {
            Ok(lock) => lock,
            Err(error) => {
                cache.writes_unavailable = true;
                return Err(error);
            }
        };
        let fresh = match self.read_fresh_vault() {
            Ok(vault) => vault,
            Err(VaultAccessError::Malformed) => {
                cache.vault = CachedVault::Malformed;
                return Err(VaultAccessError::Malformed);
            }
            Err(error) => {
                cache.writes_unavailable = true;
                return Err(error);
            }
        };
        if let Some(fresh) = fresh {
            let resolved = fresh.credentials.get(identifier).cloned().map(Secret::new);
            cache.vault = CachedVault::Loaded(fresh);
            return Ok(resolved);
        }

        let migrated_value = if authorized_legacy {
            self.keychain.get_legacy_authorized(identifier)
        } else {
            self.keychain.get_legacy(identifier)
        }
        .ok()
        .flatten()
        .map(Secret::into_inner);
        let mut migrated = CredentialVault::default();
        if let Some(value) = &migrated_value {
            migrated
                .credentials
                .insert(identifier.to_owned(), value.clone());
        }
        let encoded = encode_vault(&migrated)?;
        if self
            .keychain
            .set(KEYCHAIN_VAULT_ID, &Secret::new(encoded))
            .is_err()
        {
            cache.writes_unavailable = true;
            return Err(VaultAccessError::Unavailable);
        }

        // The cache changes only after the complete vault is durably accepted by
        // the keychain. A failed migration therefore cannot expose a transient
        // legacy value as though it had been safely migrated.
        cache.vault = CachedVault::Loaded(migrated);
        Ok(migrated_value.map(Secret::new))
    }

    fn store_in_vault(
        &self,
        identifier: &str,
        secret: &Secret<String>,
    ) -> Result<(), VaultAccessError> {
        let mut cache = self
            .vault_cache
            .lock()
            .map_err(|_| VaultAccessError::Unavailable)?;
        if cache.writes_unavailable {
            return Err(VaultAccessError::Unavailable);
        }
        match &cache.vault {
            CachedVault::Malformed => return Err(VaultAccessError::Malformed),
            CachedVault::Loaded(_) | CachedVault::Unavailable | CachedVault::Unloaded => {}
        }

        let _write_lock = match acquire_vault_write_lock() {
            Ok(lock) => lock,
            Err(error) => {
                cache.writes_unavailable = true;
                return Err(error);
            }
        };
        let fresh = match self.read_fresh_vault() {
            Ok(vault) => vault,
            Err(VaultAccessError::Malformed) => {
                cache.vault = CachedVault::Malformed;
                return Err(VaultAccessError::Malformed);
            }
            Err(error) => {
                cache.vault = CachedVault::Unavailable;
                cache.writes_unavailable = true;
                return Err(error);
            }
        };
        let mut updated = fresh.unwrap_or_default();
        cache.vault = CachedVault::Loaded(updated.clone());
        cache.legacy_bootstrap_pending = false;
        updated
            .credentials
            .insert(identifier.to_owned(), secret.expose_secret().clone());
        let encoded = encode_vault(&updated)?;
        if self
            .keychain
            .set(KEYCHAIN_VAULT_ID, &Secret::new(encoded))
            .is_err()
        {
            cache.writes_unavailable = true;
            return Err(VaultAccessError::Unavailable);
        }

        cache.vault = CachedVault::Loaded(updated);
        cache.legacy_bootstrap_pending = false;
        Ok(())
    }

    fn read_fresh_vault(&self) -> Result<Option<CredentialVault>, VaultAccessError> {
        match self.keychain.get_fresh(KEYCHAIN_VAULT_ID) {
            Ok(Some(encoded)) => decode_vault(&encoded)
                .map(Some)
                .map_err(|()| VaultAccessError::Malformed),
            Ok(None) => Ok(None),
            Err(_) => Err(VaultAccessError::Unavailable),
        }
    }

    fn load_vault(&self, cache: &mut CredentialVaultCache) -> Result<(), VaultAccessError> {
        match &cache.vault {
            CachedVault::Loaded(_) => return Ok(()),
            CachedVault::Unavailable => return Err(VaultAccessError::Unavailable),
            CachedVault::Malformed => return Err(VaultAccessError::Malformed),
            CachedVault::Unloaded => {}
        }

        match self.keychain.get(KEYCHAIN_VAULT_ID) {
            Ok(Some(encoded)) => {
                let Ok(vault) = decode_vault(&encoded) else {
                    cache.vault = CachedVault::Malformed;
                    return Err(VaultAccessError::Malformed);
                };
                cache.vault = CachedVault::Loaded(vault);
                cache.legacy_bootstrap_pending = false;
            }
            Ok(None) => {
                cache.vault = CachedVault::Loaded(CredentialVault::default());
                cache.legacy_bootstrap_pending = true;
            }
            Err(_) => {
                cache.vault = CachedVault::Unavailable;
                return Err(VaultAccessError::Unavailable);
            }
        }
        Ok(())
    }
}

fn decode_vault(encoded: &Secret<String>) -> Result<CredentialVault, ()> {
    let vault = toml::from_str::<CredentialVault>(encoded.expose_secret()).map_err(|_| ())?;
    if vault.version != KEYCHAIN_VAULT_VERSION
        || vault
            .credentials
            .keys()
            .any(|identifier| identifier.trim().is_empty())
    {
        return Err(());
    }
    Ok(vault)
}

fn encode_vault(vault: &CredentialVault) -> Result<String, VaultAccessError> {
    toml::to_string(vault).map_err(|_| VaultAccessError::Encode)
}

#[cfg(unix)]
fn acquire_vault_write_lock() -> Result<fs::File, VaultAccessError> {
    let owner = rustix::process::geteuid().as_raw();
    let directory = PathBuf::from(format!("/tmp/dev.rottweiler-credential-vault-{owner}"));
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    let created = match builder.create(&directory) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => return Err(VaultAccessError::Unavailable),
    };
    if created {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|_| VaultAccessError::Unavailable)?;
    }
    let metadata = fs::symlink_metadata(&directory).map_err(|_| VaultAccessError::Unavailable)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != owner
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(VaultAccessError::Unavailable);
    }

    let lock_file = open_private_vault_lock_file(&directory.join("write.lock"), owner)?;
    lock_file
        .lock()
        .map_err(|_| VaultAccessError::Unavailable)?;
    Ok(lock_file)
}

#[cfg(unix)]
fn open_private_vault_lock_file(path: &Path, owner: u32) -> Result<fs::File, VaultAccessError> {
    use rustix::fs::{Mode, OFlags};

    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => validate_private_lock_file(&metadata, owner)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match rustix::fs::open(
                    path,
                    OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                    Mode::RUSR | Mode::WUSR,
                ) {
                    Ok(descriptor) => {
                        let file = fs::File::from(descriptor);
                        file.set_permissions(fs::Permissions::from_mode(0o600))
                            .map_err(|_| VaultAccessError::Unavailable)?;
                        validate_private_lock_file(
                            &file.metadata().map_err(|_| VaultAccessError::Unavailable)?,
                            owner,
                        )?;
                        return Ok(file);
                    }
                    Err(rustix::io::Errno::EXIST) => continue,
                    Err(_) => return Err(VaultAccessError::Unavailable),
                }
            }
            Err(_) => return Err(VaultAccessError::Unavailable),
        }

        let descriptor = rustix::fs::open(path, OFlags::RDWR | OFlags::NOFOLLOW, Mode::empty())
            .map_err(|_| VaultAccessError::Unavailable)?;
        let file = fs::File::from(descriptor);
        validate_private_lock_file(
            &file.metadata().map_err(|_| VaultAccessError::Unavailable)?,
            owner,
        )?;
        return Ok(file);
    }
}

#[cfg(unix)]
fn validate_private_lock_file(metadata: &fs::Metadata, owner: u32) -> Result<(), VaultAccessError> {
    if !metadata.file_type().is_file()
        || metadata.uid() != owner
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(VaultAccessError::Unavailable);
    }
    Ok(())
}

#[cfg(not(unix))]
fn acquire_vault_write_lock() -> Result<fs::File, VaultAccessError> {
    // The platform-provided temporary directory is user-scoped on supported
    // non-Unix desktop targets, so the fixed name still follows vault identity.
    let directory = env::temp_dir().join("dev.rottweiler-credential-vault");
    fs::create_dir_all(&directory).map_err(|_| VaultAccessError::Unavailable)?;
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(directory.join("write.lock"))
        .map_err(|_| VaultAccessError::Unavailable)?;
    lock_file
        .lock()
        .map_err(|_| VaultAccessError::Unavailable)?;
    Ok(lock_file)
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    version: u8,
    credentials: BTreeMap<String, String>,
}

fn fallback_warning(path: &Path, identifier: &str) -> CredentialWarning {
    tracing::warn!(
        fallback_path = %path.display(),
        credential_reference = identifier,
        "using plaintext credential fallback; OS keychain storage is preferred"
    );
    CredentialWarning::PlaintextFileFallback {
        path: path.to_owned(),
    }
}

fn read_fallback(path: &Path, identifier: &str) -> Result<Option<String>, CredentialError> {
    let Some(metadata) = fallback_metadata(path)? else {
        return Ok(None);
    };
    validate_file_permissions(path, &metadata)?;

    let contents = fs::read_to_string(path).map_err(|source| CredentialError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    let file = toml::from_str::<CredentialFile>(&contents).map_err(|_| {
        CredentialError::MalformedFile {
            path: path.to_owned(),
        }
    })?;
    if file.version != CREDENTIAL_FILE_VERSION {
        return Err(CredentialError::MalformedFile {
            path: path.to_owned(),
        });
    }
    Ok(file.credentials.get(identifier).cloned())
}

fn write_fallback(path: &Path, identifier: &str, secret: &str) -> Result<(), CredentialError> {
    let mut file = if fallback_metadata(path)?.is_some() {
        read_document(path)?
    } else {
        CredentialFile {
            version: CREDENTIAL_FILE_VERSION,
            credentials: BTreeMap::new(),
        }
    };
    file.credentials
        .insert(identifier.to_owned(), secret.to_owned());
    let contents = toml::to_string(&file).map_err(|_| CredentialError::EncodeFile {
        path: path.to_owned(),
    })?;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| CredentialError::WriteFile {
            path: path.to_owned(),
            source,
        })?;
    }

    let temporary_path = fallback_temporary_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut output =
        options
            .open(&temporary_path)
            .map_err(|source| CredentialError::WriteFile {
                path: temporary_path.clone(),
                source,
            })?;
    #[cfg(unix)]
    output
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| CredentialError::WriteFile {
            path: temporary_path.clone(),
            source,
        })?;
    output
        .write_all(contents.as_bytes())
        .and_then(|()| output.sync_all())
        .map_err(|source| CredentialError::WriteFile {
            path: temporary_path.clone(),
            source,
        })?;
    drop(output);
    fs::rename(&temporary_path, path).map_err(|source| CredentialError::WriteFile {
        path: path.to_owned(),
        source,
    })
}

fn read_document(path: &Path) -> Result<CredentialFile, CredentialError> {
    let metadata = fallback_metadata(path)?.ok_or_else(|| CredentialError::ReadFile {
        path: path.to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "file no longer exists"),
    })?;
    validate_file_permissions(path, &metadata)?;
    let contents = fs::read_to_string(path).map_err(|source| CredentialError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    let file = toml::from_str::<CredentialFile>(&contents).map_err(|_| {
        CredentialError::MalformedFile {
            path: path.to_owned(),
        }
    })?;
    if file.version != CREDENTIAL_FILE_VERSION {
        return Err(CredentialError::MalformedFile {
            path: path.to_owned(),
        });
    }
    Ok(file)
}

fn fallback_metadata(path: &Path) -> Result<Option<fs::Metadata>, CredentialError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CredentialError::ReadFile {
                path: path.to_owned(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Err(CredentialError::UnsafeFileType {
            path: path.to_owned(),
        });
    }
    Ok(Some(metadata))
}

fn fallback_temporary_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map_or_else(|| "credentials".into(), std::ffi::OsString::from);
    file_name.push(".tmp");
    path.with_file_name(file_name)
}

#[cfg(unix)]
fn validate_file_permissions(path: &Path, metadata: &fs::Metadata) -> Result<(), CredentialError> {
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(CredentialError::InsecurePermissions {
            path: path.to_owned(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_file_permissions(
    _path: &Path,
    _metadata: &fs::Metadata,
) -> Result<(), CredentialError> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{Arc, Mutex};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::{
        CredentialEnvironment, CredentialError, CredentialInventoryItem, CredentialKeychain,
        CredentialManager, CredentialReference, CredentialSource, CredentialVault,
        KEYCHAIN_VAULT_ID, KeychainUnavailable, Secret, decode_vault, encode_vault,
        keychain_backend_is_file,
    };

    #[cfg(target_os = "macos")]
    use super::{
        ProcessVaultCache, ProcessVaultRead, bounded_keychain_read, read_process_vault_explicit,
    };

    #[derive(Debug, Default, Clone)]
    struct TestEnvironment(BTreeMap<String, String>);

    impl CredentialEnvironment for TestEnvironment {
        fn get(&self, name: &str) -> Result<Option<String>, CredentialError> {
            Ok(self.0.get(name).cloned())
        }
    }

    #[derive(Debug, Default)]
    struct TestKeychain {
        values: Mutex<BTreeMap<String, String>>,
        unavailable: bool,
    }

    impl TestKeychain {
        fn unavailable() -> Self {
            Self {
                values: Mutex::new(BTreeMap::new()),
                unavailable: true,
            }
        }
    }

    impl CredentialKeychain for TestKeychain {
        fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
            if self.unavailable {
                return Err(KeychainUnavailable);
            }
            let values = self.values.lock().map_err(|_| KeychainUnavailable)?;
            Ok(values.get(identifier).cloned().map(Secret::new))
        }

        fn set(
            &self,
            identifier: &str,
            secret: &Secret<String>,
        ) -> Result<(), KeychainUnavailable> {
            if self.unavailable {
                return Err(KeychainUnavailable);
            }
            let mut values = self.values.lock().map_err(|_| KeychainUnavailable)?;
            values.insert(identifier.to_owned(), secret.expose_secret().clone());
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn passive_keychain_reads_are_bounded_when_authorization_blocks() {
        let started = std::time::Instant::now();
        let result = bounded_keychain_read(std::time::Duration::from_millis(10), || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            Ok(Some(Secret::new("late-secret".to_owned())))
        });

        assert!(result.is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn authorized_wait_joins_the_in_flight_passive_keychain_read() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let background_calls = Arc::clone(&calls);
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let operation = ProcessVaultRead::start(move || {
            background_calls.fetch_add(1, Ordering::AcqRel);
            released.recv().map_err(|_| KeychainUnavailable)?;
            Ok(Some(Secret::new("shared-result".to_owned())))
        })
        .expect("start shared read");

        assert!(
            operation
                .wait(Some(std::time::Duration::from_millis(5)))
                .is_err(),
            "passive inventory should remain bounded"
        );
        let authorized = Arc::clone(&operation);
        let waiter = std::thread::spawn(move || authorized.wait(None));
        release.send(()).expect("release keychain fixture");
        assert_eq!(
            waiter
                .join()
                .expect("authorized waiter")
                .expect("shared keychain result"),
            Some("shared-result".to_owned())
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fresh_store_read_joins_a_blocked_passive_read_without_a_second_prompt() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let passive_calls = Arc::new(AtomicUsize::new(0));
        let background_calls = Arc::clone(&passive_calls);
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let passive = ProcessVaultRead::start(move || {
            background_calls.fetch_add(1, Ordering::AcqRel);
            released.recv().map_err(|_| KeychainUnavailable)?;
            Ok(Some(Secret::new("vault-before-store".to_owned())))
        })
        .expect("start blocked passive read");
        assert!(
            passive
                .wait(Some(std::time::Duration::from_millis(5)))
                .is_err(),
            "the passive read should still be blocked when storage starts"
        );

        let cache = Arc::new(Mutex::new(ProcessVaultCache::Reading(passive)));
        let fresh_starts = Arc::new(AtomicUsize::new(0));
        let waiter_cache = Arc::clone(&cache);
        let waiter_starts = Arc::clone(&fresh_starts);
        let store_read = std::thread::spawn(move || {
            read_process_vault_explicit(&waiter_cache, false, move || {
                waiter_starts.fetch_add(1, Ordering::AcqRel);
                Ok(Some(Secret::new("unexpected-second-read".to_owned())))
            })
        });

        release.send(()).expect("release passive keychain fixture");
        let value = store_read
            .join()
            .expect("fresh store waiter")
            .expect("shared keychain result")
            .expect("vault should exist");
        assert_eq!(value.expose_secret(), "vault-before-store");
        assert_eq!(passive_calls.load(Ordering::Acquire), 1);
        assert_eq!(fresh_starts.load(Ordering::Acquire), 0);

        let refresh_starts = Arc::clone(&fresh_starts);
        let refreshed = read_process_vault_explicit(&cache, false, move || {
            refresh_starts.fetch_add(1, Ordering::AcqRel);
            Ok(Some(Secret::new("vault-after-other-writer".to_owned())))
        })
        .expect("loaded state must still receive a fresh store read")
        .expect("refreshed vault should exist");
        assert_eq!(refreshed.expose_secret(), "vault-after-other-writer");
        assert_eq!(fresh_starts.load(Ordering::Acquire), 1);

        *cache.lock().expect("test process cache should lock") = ProcessVaultCache::Unavailable;
        let retry_starts = Arc::clone(&fresh_starts);
        let retried = read_process_vault_explicit(&cache, false, move || {
            retry_starts.fetch_add(1, Ordering::AcqRel);
            Ok(Some(Secret::new("vault-after-retry".to_owned())))
        })
        .expect("a failed explicit read must remain retryable")
        .expect("retried vault should exist");
        assert_eq!(retried.expose_secret(), "vault-after-retry");
        assert_eq!(fresh_starts.load(Ordering::Acquire), 2);
    }

    #[derive(Clone, Default)]
    struct RecordingKeychain(Arc<Mutex<RecordingKeychainState>>);

    #[derive(Default)]
    struct RecordingKeychainState {
        vault: Option<String>,
        legacy: BTreeMap<String, String>,
        calls: Vec<String>,
        vault_get_unavailable: bool,
        vault_set_unavailable: bool,
        legacy_get_unavailable: bool,
    }

    impl RecordingKeychain {
        fn with_vault(vault: &CredentialVault) -> Self {
            let encoded =
                encode_vault(vault).unwrap_or_else(|_| panic!("test vault should encode"));
            let keychain = Self::default();
            keychain
                .0
                .lock()
                .expect("recording keychain should lock")
                .vault = Some(encoded);
            keychain
        }

        fn insert_legacy(&self, identifier: &str, value: &str) {
            self.0
                .lock()
                .expect("recording keychain should lock")
                .legacy
                .insert(identifier.to_owned(), value.to_owned());
        }

        fn calls(&self) -> Vec<String> {
            self.0
                .lock()
                .expect("recording keychain should lock")
                .calls
                .clone()
        }

        fn decoded_vault(&self) -> Option<CredentialVault> {
            self.0
                .lock()
                .expect("recording keychain should lock")
                .vault
                .as_ref()
                .map(|encoded| {
                    decode_vault(&Secret::new(encoded.clone()))
                        .expect("recorded test vault should decode")
                })
        }
    }

    impl CredentialKeychain for RecordingKeychain {
        fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
            let mut state = self.0.lock().map_err(|_| KeychainUnavailable)?;
            state.calls.push(format!("get:{identifier}"));
            if state.vault_get_unavailable {
                return Err(KeychainUnavailable);
            }
            Ok(state.vault.clone().map(Secret::new))
        }

        fn set(
            &self,
            identifier: &str,
            secret: &Secret<String>,
        ) -> Result<(), KeychainUnavailable> {
            let mut state = self.0.lock().map_err(|_| KeychainUnavailable)?;
            state.calls.push(format!("set:{identifier}"));
            if state.vault_set_unavailable {
                return Err(KeychainUnavailable);
            }
            state.vault = Some(secret.expose_secret().clone());
            Ok(())
        }

        fn get_authorized(
            &self,
            identifier: &str,
        ) -> Result<Option<Secret<String>>, KeychainUnavailable> {
            let mut state = self.0.lock().map_err(|_| KeychainUnavailable)?;
            state.calls.push(format!("get-authorized:{identifier}"));
            Ok(state.vault.clone().map(Secret::new))
        }

        fn get_fresh(
            &self,
            identifier: &str,
        ) -> Result<Option<Secret<String>>, KeychainUnavailable> {
            let mut state = self.0.lock().map_err(|_| KeychainUnavailable)?;
            state.calls.push(format!("get-fresh:{identifier}"));
            if state.vault_get_unavailable {
                return Err(KeychainUnavailable);
            }
            Ok(state.vault.clone().map(Secret::new))
        }

        fn get_legacy(
            &self,
            identifier: &str,
        ) -> Result<Option<Secret<String>>, KeychainUnavailable> {
            let mut state = self.0.lock().map_err(|_| KeychainUnavailable)?;
            state.calls.push(format!("get-legacy:{identifier}"));
            if state.legacy_get_unavailable {
                return Err(KeychainUnavailable);
            }
            Ok(state.legacy.get(identifier).cloned().map(Secret::new))
        }

        fn get_legacy_authorized(
            &self,
            identifier: &str,
        ) -> Result<Option<Secret<String>>, KeychainUnavailable> {
            let mut state = self.0.lock().map_err(|_| KeychainUnavailable)?;
            state
                .calls
                .push(format!("get-legacy-authorized:{identifier}"));
            if state.legacy_get_unavailable {
                return Err(KeychainUnavailable);
            }
            Ok(state.legacy.get(identifier).cloned().map(Secret::new))
        }
    }

    #[test]
    fn empty_inventory_skips_keychain_legacy_and_fallback_access() {
        let root = tempdir().expect("temporary root should be created");
        let keychain = RecordingKeychain::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            // A directory is intentionally unsafe as a fallback file. Success
            // therefore proves the fallback metadata/document path was skipped.
            root.path(),
        );
        let inventory = manager
            .resolve_inventory(&[])
            .expect("empty inventory should be side-effect free");
        assert!(inventory.is_empty());
        assert!(keychain.calls().is_empty());
    }

    #[test]
    fn environment_satisfied_inventory_skips_keychain_legacy_and_fallback_access() {
        let root = tempdir().expect("temporary root should be created");
        let keychain = RecordingKeychain::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment(BTreeMap::from([(
                "RW_INVENTORY_TOKEN".to_owned(),
                "environment-token".to_owned(),
            )])),
            keychain.clone(),
            // As above, touching this directory as a fallback file would fail.
            root.path(),
        );
        let references = [
            CredentialReference::new("first").with_environment("RW_INVENTORY_TOKEN"),
            CredentialReference::new("second").with_environment("RW_INVENTORY_TOKEN"),
        ];
        let inventory = manager
            .resolve_inventory(&references)
            .expect("environment-only inventory should avoid durable stores");
        assert_eq!(inventory.len(), 2);
        assert!(inventory.iter().all(|item| matches!(
            item,
            CredentialInventoryItem::Present(resolved)
                if matches!(resolved.source(), CredentialSource::Environment(_))
        )));
        assert!(keychain.calls().is_empty());
    }

    #[test]
    fn missing_inventory_reads_one_vault_without_writes_or_legacy_migration() {
        let root = tempdir().expect("temporary root should be created");
        let keychain = RecordingKeychain::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("missing-credentials.toml"),
        );
        let inventory = manager
            .resolve_inventory(&[
                CredentialReference::new("first"),
                CredentialReference::new("second"),
            ])
            .expect("missing inventory should remain non-destructive");
        assert!(
            inventory
                .iter()
                .all(|item| matches!(item, CredentialInventoryItem::Missing))
        );
        assert_eq!(keychain.calls(), vec![format!("get:{KEYCHAIN_VAULT_ID}")]);
    }

    #[test]
    fn authorized_resolution_retries_once_after_passive_vault_failure() {
        let root = tempdir().expect("temporary root should be created");
        let mut vault = CredentialVault::default();
        vault
            .credentials
            .insert("first".to_owned(), "secret-one".to_owned());
        vault
            .credentials
            .insert("second".to_owned(), "secret-two".to_owned());
        let keychain = RecordingKeychain::with_vault(&vault);
        keychain
            .0
            .lock()
            .expect("recording keychain should lock")
            .vault_get_unavailable = true;
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        assert!(matches!(
            manager.resolve(&CredentialReference::new("first")),
            Err(CredentialError::KeychainUnavailable { .. })
        ));
        let first = manager
            .resolve_authorized(&CredentialReference::new("first"))
            .expect("authorized resolution should retry the vault");
        let second = manager
            .resolve_authorized(&CredentialReference::new("second"))
            .expect("loaded vault should satisfy another logical credential");

        assert_eq!(first.secret().expose_secret(), "secret-one");
        assert_eq!(second.secret().expose_secret(), "secret-two");
        assert_eq!(
            keychain.calls(),
            vec![
                format!("get:{KEYCHAIN_VAULT_ID}"),
                format!("get-authorized:{KEYCHAIN_VAULT_ID}"),
            ]
        );
    }

    #[test]
    fn authorized_resolution_preserves_one_time_legacy_vault_migration() {
        let root = tempdir().expect("temporary root should be created");
        let keychain = RecordingKeychain::default();
        keychain.insert_legacy("provider-token", "legacy-provider-secret");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        let resolved = manager
            .resolve_authorized(&CredentialReference::new("provider-token"))
            .expect("authorized active resolution should migrate a legacy credential");

        assert_eq!(resolved.secret().expose_secret(), "legacy-provider-secret");
        assert_eq!(resolved.source(), &CredentialSource::OsKeychain);
        assert_eq!(
            keychain.calls(),
            vec![
                format!("get-authorized:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                "get-legacy-authorized:provider-token".to_owned(),
                format!("set:{KEYCHAIN_VAULT_ID}"),
            ]
        );
    }

    #[test]
    fn credential_backend_environment_is_explicit_and_fails_closed() {
        assert!(!keychain_backend_is_file(None));
        assert!(!keychain_backend_is_file(Some(std::ffi::OsStr::new(
            "keychain"
        ))));
        assert!(keychain_backend_is_file(Some(std::ffi::OsStr::new("file"))));
        assert!(keychain_backend_is_file(Some(std::ffi::OsStr::new(
            "invalid"
        ))));
    }

    #[test]
    fn environment_wins_over_keychain_and_file() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("credentials.toml");
        let environment = TestEnvironment(BTreeMap::from([(
            "RW_TEST_TOKEN".to_owned(),
            "from-environment".to_owned(),
        )]));
        let keychain = TestKeychain::default();
        keychain
            .set("primary", &Secret::new("from-keychain".to_owned()))
            .expect("test keychain should accept a value");
        let manager = CredentialManager::with_backends(environment, keychain, path);
        let reference = CredentialReference::new("primary").with_environment("RW_TEST_TOKEN");

        let resolved = manager
            .resolve(&reference)
            .expect("environment credential should resolve");

        assert_eq!(resolved.secret().expose_secret(), "from-environment");
        assert_eq!(
            resolved.source(),
            &CredentialSource::Environment("RW_TEST_TOKEN".to_owned())
        );
        assert!(resolved.warnings().is_empty());
    }

    #[test]
    fn keychain_wins_when_environment_is_absent() {
        let root = tempdir().expect("temporary directory should be created");
        let keychain = TestKeychain::default();
        keychain
            .set("primary", &Secret::new("from-keychain".to_owned()))
            .expect("test keychain should accept a value");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain,
            root.path().join("credentials.toml"),
        );

        let resolved = manager
            .resolve(&CredentialReference::new("primary"))
            .expect("keychain credential should resolve");

        assert_eq!(resolved.secret().expose_secret(), "from-keychain");
        assert_eq!(resolved.source(), &CredentialSource::OsKeychain);
        assert!(resolved.warnings().is_empty());
    }

    #[test]
    fn all_logical_credentials_share_one_cached_vault_item() {
        let root = tempdir().expect("temporary directory should be created");
        let keychain = RecordingKeychain::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        for (identifier, value) in [("provider-a", "secret-a"), ("proxy-b", "secret-b")] {
            let stored = manager
                .store(
                    &CredentialReference::new(identifier),
                    &Secret::new(value.to_owned()),
                )
                .expect("vault credential should store");
            assert_eq!(stored.source(), &CredentialSource::OsKeychain);
        }

        for (identifier, value) in [("provider-a", "secret-a"), ("proxy-b", "secret-b")] {
            let resolved = manager
                .resolve(&CredentialReference::new(identifier))
                .expect("cached vault credential should resolve");
            assert_eq!(resolved.secret().expose_secret(), value);
        }

        assert_eq!(
            keychain.calls(),
            vec![
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                format!("set:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                format!("set:{KEYCHAIN_VAULT_ID}"),
            ]
        );
        let vault = keychain
            .decoded_vault()
            .expect("single vault item should exist");
        assert_eq!(vault.credentials.len(), 2);
        assert_eq!(
            vault.credentials.get("provider-a"),
            Some(&"secret-a".into())
        );
        assert_eq!(vault.credentials.get("proxy-b"), Some(&"secret-b".into()));
    }

    #[test]
    fn independent_managers_fresh_merge_instead_of_overwriting_stale_vaults() {
        let root = tempdir().expect("temporary directory should be created");
        let initial = CredentialVault {
            credentials: BTreeMap::from([("base".to_owned(), "base-secret".to_owned())]),
            ..CredentialVault::default()
        };
        let keychain = RecordingKeychain::with_vault(&initial);
        let first = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("first/credentials.toml"),
        );
        let second = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("second/other-fallback.toml"),
        );

        first
            .resolve(&CredentialReference::new("base"))
            .expect("first independent manager should cache the original vault");
        second
            .resolve(&CredentialReference::new("base"))
            .expect("second independent manager should cache the original vault");

        first
            .store(
                &CredentialReference::new("provider-a"),
                &Secret::new("secret-a".to_owned()),
            )
            .expect("first manager should write the vault");
        second
            .store(
                &CredentialReference::new("provider-b"),
                &Secret::new("secret-b".to_owned()),
            )
            .expect("second manager should merge into the shared vault");

        let vault = keychain
            .decoded_vault()
            .expect("shared vault should remain available");
        assert_eq!(vault.credentials.get("base"), Some(&"base-secret".into()));
        assert_eq!(
            vault.credentials.get("provider-a"),
            Some(&"secret-a".into())
        );
        assert_eq!(
            vault.credentials.get("provider-b"),
            Some(&"secret-b".into())
        );
        assert_eq!(
            keychain.calls(),
            vec![
                format!("get:{KEYCHAIN_VAULT_ID}"),
                format!("get:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                format!("set:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                format!("set:{KEYCHAIN_VAULT_ID}"),
            ]
        );
    }

    #[test]
    fn existing_vault_is_read_once_and_missing_ids_never_probe_legacy_entries() {
        let root = tempdir().expect("temporary directory should be created");
        let vault = CredentialVault {
            credentials: BTreeMap::from([
                ("provider-a".to_owned(), "secret-a".to_owned()),
                ("proxy-b".to_owned(), "secret-b".to_owned()),
            ]),
            ..CredentialVault::default()
        };
        let keychain = RecordingKeychain::with_vault(&vault);
        keychain.insert_legacy("missing-a", "must-not-be-read");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        for identifier in ["provider-a", "proxy-b"] {
            manager
                .resolve(&CredentialReference::new(identifier))
                .expect("existing vault credential should resolve");
        }
        for identifier in ["missing-a", "missing-b", "missing-c"] {
            assert!(matches!(
                manager.resolve(&CredentialReference::new(identifier)),
                Err(CredentialError::NotFound { .. })
            ));
        }

        assert_eq!(keychain.calls(), vec![format!("get:{KEYCHAIN_VAULT_ID}")]);
    }

    #[test]
    fn virgin_vault_migrates_only_the_first_requested_legacy_identifier() {
        let root = tempdir().expect("temporary directory should be created");
        let keychain = RecordingKeychain::default();
        keychain.insert_legacy("first", "legacy-first");
        keychain.insert_legacy("second", "legacy-second-must-not-be-read");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        let migrated = manager
            .resolve(&CredentialReference::new("first"))
            .expect("first legacy credential should migrate");
        assert_eq!(migrated.secret().expose_secret(), "legacy-first");
        assert_eq!(migrated.source(), &CredentialSource::OsKeychain);
        assert!(matches!(
            manager.resolve(&CredentialReference::new("second")),
            Err(CredentialError::NotFound { .. })
        ));

        assert_eq!(
            keychain.calls(),
            vec![
                format!("get:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                "get-legacy:first".to_owned(),
                format!("set:{KEYCHAIN_VAULT_ID}"),
            ]
        );
        let vault = keychain
            .decoded_vault()
            .expect("migrated vault should be durable");
        assert_eq!(vault.credentials.get("first"), Some(&"legacy-first".into()));
        assert!(!vault.credentials.contains_key("second"));
    }

    #[test]
    fn empty_legacy_bootstrap_is_persisted_and_never_repeated() {
        let root = tempdir().expect("temporary directory should be created");
        let keychain = RecordingKeychain::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        for identifier in ["first-missing", "second-missing", "third-missing"] {
            assert!(matches!(
                manager.resolve(&CredentialReference::new(identifier)),
                Err(CredentialError::NotFound { .. })
            ));
        }

        assert_eq!(
            keychain.calls(),
            vec![
                format!("get:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                "get-legacy:first-missing".to_owned(),
                format!("set:{KEYCHAIN_VAULT_ID}"),
            ]
        );
        assert!(
            keychain
                .decoded_vault()
                .expect("empty bootstrap vault should be durable")
                .credentials
                .is_empty()
        );
    }

    #[test]
    fn denied_legacy_lookup_persists_empty_marker_without_poisoning_new_vault() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("credentials.toml");
        let keychain = RecordingKeychain::default();
        keychain
            .0
            .lock()
            .expect("recording keychain should lock")
            .legacy_get_unavailable = true;
        let first = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            path.clone(),
        );

        assert!(matches!(
            first.resolve(&CredentialReference::new("first-missing")),
            Err(CredentialError::NotFound { .. })
        ));
        assert!(
            keychain
                .decoded_vault()
                .expect("denied migration should still persist an empty marker")
                .credentials
                .is_empty()
        );

        let second =
            CredentialManager::with_backends(TestEnvironment::default(), keychain.clone(), path);
        assert!(matches!(
            second.resolve(&CredentialReference::new("second-missing")),
            Err(CredentialError::NotFound { .. })
        ));
        assert_eq!(
            keychain.calls(),
            vec![
                format!("get:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                "get-legacy:first-missing".to_owned(),
                format!("set:{KEYCHAIN_VAULT_ID}"),
                format!("get:{KEYCHAIN_VAULT_ID}"),
            ]
        );
    }

    #[test]
    fn failed_legacy_migration_never_exposes_an_undurable_secret() {
        const CANARY: &str = "rw-legacy-migration-canary";
        let root = tempdir().expect("temporary directory should be created");
        let keychain = RecordingKeychain::default();
        keychain.insert_legacy("first", CANARY);
        keychain
            .0
            .lock()
            .expect("recording keychain should lock")
            .vault_set_unavailable = true;
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        for _ in 0..2 {
            let error = manager
                .resolve(&CredentialReference::new("first"))
                .expect_err("undurable migration must fail closed");
            assert!(matches!(error, CredentialError::KeychainUnavailable { .. }));
            assert!(!error.to_string().contains(CANARY));
            assert!(!format!("{error:?}").contains(CANARY));
        }

        assert_eq!(
            keychain.calls(),
            vec![
                format!("get:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                "get-legacy:first".to_owned(),
                format!("set:{KEYCHAIN_VAULT_ID}"),
            ]
        );
        assert!(keychain.decoded_vault().is_none());
    }

    #[test]
    fn malformed_vault_is_sanitized_and_never_overwritten_or_bypassed() {
        const CANARY: &str = "rw-malformed-vault-canary";
        let root = tempdir().expect("temporary directory should be created");
        let fallback_path = root.path().join("credentials.toml");
        super::write_fallback(&fallback_path, "primary", "fallback-must-not-win")
            .expect("fallback fixture should be written");
        let keychain = RecordingKeychain::default();
        keychain
            .0
            .lock()
            .expect("recording keychain should lock")
            .vault = Some(format!("malformed {CANARY}"));
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            fallback_path,
        );

        let resolve_error = manager
            .resolve(&CredentialReference::new("primary"))
            .expect_err("malformed vault must fail closed before fallback");
        assert!(matches!(
            resolve_error,
            CredentialError::MalformedKeychainVault
        ));
        assert!(!resolve_error.to_string().contains(CANARY));

        let store_error = manager
            .store(
                &CredentialReference::new("primary"),
                &Secret::new("replacement".to_owned()),
            )
            .expect_err("malformed vault must not be overwritten");
        assert!(matches!(
            store_error,
            CredentialError::MalformedKeychainVault
        ));
        assert_eq!(keychain.calls(), vec![format!("get:{KEYCHAIN_VAULT_ID}")]);
    }

    #[test]
    fn unavailable_vault_access_is_cached_for_the_manager_lifetime() {
        let root = tempdir().expect("temporary directory should be created");
        let keychain = RecordingKeychain::default();
        keychain
            .0
            .lock()
            .expect("recording keychain should lock")
            .vault_get_unavailable = true;
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        for identifier in ["provider-a", "proxy-b", "provider-c"] {
            assert!(matches!(
                manager.resolve(&CredentialReference::new(identifier)),
                Err(CredentialError::KeychainUnavailable { .. })
            ));
        }

        assert_eq!(keychain.calls(), vec![format!("get:{KEYCHAIN_VAULT_ID}")]);
    }

    #[test]
    fn explicit_store_retries_after_a_passive_vault_read_was_unavailable() {
        let root = tempdir().expect("temporary directory should be created");
        let keychain = RecordingKeychain::default();
        keychain
            .0
            .lock()
            .expect("recording keychain should lock")
            .vault_get_unavailable = true;
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            root.path().join("credentials.toml"),
        );

        assert!(matches!(
            manager.resolve(&CredentialReference::new("provider-a")),
            Err(CredentialError::KeychainUnavailable { .. })
        ));
        keychain
            .0
            .lock()
            .expect("recording keychain should lock")
            .vault_get_unavailable = false;

        let stored = manager
            .store(
                &CredentialReference::new("provider-a"),
                &Secret::new("replacement".to_owned()),
            )
            .expect("an explicit store should retry the secure vault");

        assert_eq!(stored.source(), &CredentialSource::OsKeychain);
        assert_eq!(
            keychain.calls(),
            vec![
                format!("get:{KEYCHAIN_VAULT_ID}"),
                format!("get-fresh:{KEYCHAIN_VAULT_ID}"),
                format!("set:{KEYCHAIN_VAULT_ID}"),
            ]
        );
    }

    #[test]
    fn unavailable_keychain_uses_mode_0600_file_with_typed_warning() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("private").join("credentials.toml");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            TestKeychain::unavailable(),
            path.clone(),
        );
        let reference = CredentialReference::new("primary");

        let stored = manager
            .store(&reference, &Secret::new("file-secret".to_owned()))
            .expect("fallback credential should be stored");
        assert_eq!(
            stored.source(),
            &CredentialSource::FallbackFile(path.clone())
        );
        assert_eq!(stored.warnings().len(), 1);

        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path)
                .expect("credential file should exist")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let resolved = manager
            .resolve(&reference)
            .expect("fallback credential should resolve");
        assert_eq!(resolved.secret().expose_secret(), "file-secret");
        assert_eq!(resolved.warnings().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn insecure_fallback_permissions_fail_closed() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("credentials.toml");
        fs::write(
            &path,
            "version = 1\n[credentials]\nprimary = \"file-secret\"\n",
        )
        .expect("credential fixture should be written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("credential fixture permissions should change");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            TestKeychain::unavailable(),
            path,
        );

        let error = manager
            .resolve(&CredentialReference::new("primary"))
            .expect_err("world-readable credential file must be rejected");

        assert!(matches!(error, CredentialError::InsecurePermissions { .. }));
    }

    #[test]
    fn diagnostics_never_expose_secret_canaries() {
        const CANARY: &str = "rw-secret-canary-do-not-leak";
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("credentials.toml");
        let secret = Secret::new(CANARY.to_owned());

        assert!(!format!("{secret:?}").contains(CANARY));
        assert!(!secret.to_string().contains(CANARY));

        fs::write(&path, format!("this is malformed {CANARY}"))
            .expect("malformed credential fixture should be written");
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("credential fixture should be private");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            TestKeychain::unavailable(),
            path,
        );
        let error = manager
            .resolve(&CredentialReference::new("primary"))
            .expect_err("malformed credential file must fail");
        assert!(!format!("{error:?}").contains(CANARY));
        assert!(!error.to_string().contains(CANARY));
    }
}
