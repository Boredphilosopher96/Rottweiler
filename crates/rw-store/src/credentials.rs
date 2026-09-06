//! Provider-blind credential lookup and storage.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable identifier of the injected credential vault used by tests and
/// embedders. Production storage is always the owner-private credential file.
pub const CREDENTIAL_VAULT_ID: &str = "credentials";

const CREDENTIAL_VAULT_VERSION: u8 = 1;
const CREDENTIAL_FILE_VERSION: u8 = 1;

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
    /// Creates a credential reference without an environment override.
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

    /// Stable identifier used by the credential store or credential file.
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
    /// An injected credential store used by tests or embedders.
    InjectedStore,
    /// Rottweiler's owner-private credential file.
    CredentialFile(PathBuf),
}

/// Security warnings that callers must surface to the user.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CredentialWarning {
    /// An injected store fell back to the owner-private credential file.
    #[error("credential is stored in owner-private file {path} (mode 0600)")]
    OwnerPrivateCredentialFile {
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
    /// The injected credential store was unavailable and no file value exists.
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

    /// Winning source after applying environment, injected-store, then file precedence.
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
    /// An injected credential store could not be accessed and the file had no value.
    #[error("credential store is unavailable for credential {identifier:?}")]
    CredentialStoreUnavailable {
        /// Non-secret reference identifier.
        identifier: String,
    },
    /// An injected credential store document could not be decoded safely.
    #[error("credential store document is malformed")]
    MalformedCredentialStore,
    /// An injected credential store document could not be encoded safely.
    #[error("could not encode credential store document")]
    EncodeCredentialStore,
    /// A credential file had unsafe group/other permissions.
    #[error("credential file {path} has insecure permissions {mode:#o}; expected 0600")]
    InsecurePermissions {
        /// Insecure file.
        path: PathBuf,
        /// Observed Unix permission bits.
        mode: u32,
    },
    /// A credential file could not be read.
    #[error("could not read credential file {path}: {source}")]
    ReadFile {
        /// File path.
        path: PathBuf,
        /// Underlying I/O error, which contains no file contents.
        #[source]
        source: std::io::Error,
    },
    /// A credential file was malformed. The parser source is suppressed to prevent excerpts.
    #[error("credential file {path} is malformed")]
    MalformedFile {
        /// File path.
        path: PathBuf,
    },
    /// A credential path was not a regular file (for example, it was a symlink).
    #[error("credential file path {path} is not a regular file")]
    UnsafeFileType {
        /// Unsafe path.
        path: PathBuf,
    },
    /// A credential file could not be securely written.
    #[error("could not write credential file {path}: {source}")]
    WriteFile {
        /// File path.
        path: PathBuf,
        /// Underlying I/O error, which contains no credential contents.
        #[source]
        source: std::io::Error,
    },
    /// The in-memory credential document could not be encoded.
    #[error("could not encode credential file {path}")]
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

/// Sanitized credential-store outcome used by injected test backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("credential store is unavailable")]
pub struct CredentialStoreUnavailable;

/// Injectable secure-credential-store boundary.
pub trait CredentialStore {
    /// Reads one credential-store item, returning `None` when no entry exists.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialStoreUnavailable`] without exposing backend diagnostics.
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, CredentialStoreUnavailable>;

    /// Reads one credential-store item for an explicit, user-initiated operation.
    ///
    /// Test backends that do not distinguish active and passive reads may use
    /// the default implementation.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialStoreUnavailable`] without exposing backend diagnostics.
    fn get_authorized(
        &self,
        identifier: &str,
    ) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
        self.get(identifier)
    }

    /// Creates or replaces a credential.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialStoreUnavailable`] without exposing backend diagnostics.
    fn set(
        &self,
        identifier: &str,
        secret: &Secret<String>,
    ) -> Result<(), CredentialStoreUnavailable>;

    /// Reads the current vault value without using a process cache.
    ///
    /// Whole-vault mutations use this only while holding the cross-process vault
    /// lock, so they merge against the latest durable map without adding reads to
    /// ordinary credential resolution.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialStoreUnavailable`] without exposing backend diagnostics.
    fn get_fresh(
        &self,
        identifier: &str,
    ) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
        self.get(identifier)
    }
}

/// Empty injected-store boundary used by production. Provider credentials are
/// persisted only in the owner-private credential file.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoExternalCredentialStore;

impl CredentialStore for NoExternalCredentialStore {
    fn get(&self, _identifier: &str) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
        Ok(None)
    }

    fn set(
        &self,
        _identifier: &str,
        _secret: &Secret<String>,
    ) -> Result<(), CredentialStoreUnavailable> {
        Err(CredentialStoreUnavailable)
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
            version: CREDENTIAL_VAULT_VERSION,
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
    writes_unavailable: bool,
}

impl Default for CredentialVaultCache {
    fn default() -> Self {
        Self {
            vault: CachedVault::Unloaded,
            writes_unavailable: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum VaultAccessError {
    Unavailable,
    Malformed,
    Encode,
}

/// Credential manager with injectable environment and credential-store boundaries.
pub struct CredentialManager<E = SystemEnvironment, K = NoExternalCredentialStore> {
    environment: E,
    store: K,
    credential_file_path: PathBuf,
    vault_cache: Arc<Mutex<CredentialVaultCache>>,
    warn_on_credential_file: bool,
    use_injected_store: bool,
}

impl<E, K> Clone for CredentialManager<E, K>
where
    E: Clone,
    K: Clone,
{
    fn clone(&self) -> Self {
        Self {
            environment: self.environment.clone(),
            store: self.store.clone(),
            credential_file_path: self.credential_file_path.clone(),
            vault_cache: self.vault_cache.clone(),
            warn_on_credential_file: self.warn_on_credential_file,
            use_injected_store: self.use_injected_store,
        }
    }
}

fn process_credential_vault_cache() -> Arc<Mutex<CredentialVaultCache>> {
    static CACHE: OnceLock<Arc<Mutex<CredentialVaultCache>>> = OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(Mutex::new(CredentialVaultCache::default())))
        .clone()
}

impl CredentialManager<SystemEnvironment, NoExternalCredentialStore> {
    /// Creates the production manager using the process environment and the
    /// owner-private credential file. No operating-system credential store is used.
    #[must_use]
    pub fn system(credential_file_path: impl Into<PathBuf>) -> Self {
        Self {
            environment: SystemEnvironment,
            store: NoExternalCredentialStore,
            credential_file_path: credential_file_path.into(),
            vault_cache: process_credential_vault_cache(),
            warn_on_credential_file: false,
            use_injected_store: false,
        }
    }
}

impl<E, K> CredentialManager<E, K>
where
    E: CredentialEnvironment,
    K: CredentialStore,
{
    /// Inventories many references with one injected-store read and at most one
    /// credential-file read. Inventory is read-only.
    ///
    /// Returned entries align exactly with `references`; secret values remain
    /// inside [`ResolvedCredential`] and its redacted debug boundary.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for an invalid reference/environment value,
    /// malformed vault/credential document, or unsafe credential file.
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
        let (vault, store_unavailable) = {
            let mut cache = self.vault_cache.lock().map_err(|_| {
                CredentialError::CredentialStoreUnavailable {
                    identifier: "credential-vault".to_owned(),
                }
            })?;
            let unavailable = match self.load_vault(&mut cache) {
                Ok(()) => false,
                Err(VaultAccessError::Unavailable) => true,
                Err(VaultAccessError::Malformed) => {
                    return Err(CredentialError::MalformedCredentialStore);
                }
                Err(VaultAccessError::Encode) => {
                    return Err(CredentialError::EncodeCredentialStore);
                }
            };
            let values = match &cache.vault {
                CachedVault::Loaded(vault) => vault.credentials.clone(),
                CachedVault::Unavailable | CachedVault::Unloaded => BTreeMap::new(),
                CachedVault::Malformed => return Err(CredentialError::MalformedCredentialStore),
            };
            (values, unavailable)
        };
        let credential_file_document =
            if credential_file_metadata(&self.credential_file_path)?.is_some() {
                Some(read_document(&self.credential_file_path)?)
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
                        source: CredentialSource::InjectedStore,
                        warnings: Vec::new(),
                    });
                }
                if let Some(value) = credential_file_document
                    .as_ref()
                    .and_then(|document| document.credentials.get(reference.identifier()))
                {
                    return CredentialInventoryItem::Present(ResolvedCredential {
                        secret: Secret::new(value.clone()),
                        source: CredentialSource::CredentialFile(self.credential_file_path.clone()),
                        warnings: self.file_warnings(reference.identifier()),
                    });
                }
                if store_unavailable {
                    CredentialInventoryItem::StoreUnavailable
                } else {
                    CredentialInventoryItem::Missing
                }
            })
            .collect())
    }

    /// Creates a manager with deterministic/injectable external boundaries.
    #[must_use]
    pub fn with_backends(
        environment: E,
        store: K,
        credential_file_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            environment,
            store,
            credential_file_path: credential_file_path.into(),
            vault_cache: Arc::new(Mutex::new(CredentialVaultCache::default())),
            warn_on_credential_file: true,
            use_injected_store: true,
        }
    }

    /// Resolves environment first, then the injected credential store and
    /// owner-private file. Production uses only environment and the file.
    ///
    /// # Errors
    ///
    /// Returns a sanitized [`CredentialError`] for invalid references, unavailable
    /// sources, insecure credential-file permissions, or unreadable file data.
    pub fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<ResolvedCredential, CredentialError> {
        self.resolve_with_mode(reference, false)
    }

    /// Resolves a credential for an explicit active operation.
    ///
    /// Production resolution never opens an operating-system authorization
    /// dialog. Injected embedders may distinguish active and passive reads.
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

        let store = if authorized {
            self.resolve_from_vault_authorized(reference.identifier())
        } else {
            self.resolve_from_vault(reference.identifier())
        };
        let store_unavailable = match store {
            Ok(Some(secret)) => {
                return Ok(ResolvedCredential {
                    secret,
                    source: CredentialSource::InjectedStore,
                    warnings: Vec::new(),
                });
            }
            Ok(None) => false,
            Err(VaultAccessError::Unavailable) => true,
            Err(VaultAccessError::Malformed) => {
                return Err(CredentialError::MalformedCredentialStore);
            }
            Err(VaultAccessError::Encode) => {
                return Err(CredentialError::EncodeCredentialStore);
            }
        };

        if let Some(secret) =
            read_credential_file_value(&self.credential_file_path, reference.identifier())?
        {
            return Ok(ResolvedCredential {
                secret: Secret::new(secret),
                source: CredentialSource::CredentialFile(self.credential_file_path.clone()),
                warnings: self.file_warnings(reference.identifier()),
            });
        }

        if store_unavailable {
            Err(CredentialError::CredentialStoreUnavailable {
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
                return Ok(vault.credentials.get(identifier).cloned().map(Secret::new));
            }
            CachedVault::Malformed => return Err(VaultAccessError::Malformed),
            CachedVault::Unloaded | CachedVault::Unavailable => {}
        }
        let encoded = self
            .store
            .get_authorized(CREDENTIAL_VAULT_ID)
            .map_err(|_| VaultAccessError::Unavailable)?;
        let vault = match encoded {
            Some(encoded) => decode_vault(&encoded).map_err(|()| VaultAccessError::Malformed)?,
            None => CredentialVault::default(),
        };
        let value = vault.credentials.get(identifier).cloned().map(Secret::new);
        cache.vault = CachedVault::Loaded(vault);
        Ok(value)
    }

    /// Stores in the injected credential store or owner-private file.
    /// Production always selects the file.
    ///
    /// # Errors
    ///
    /// Returns a sanitized [`CredentialError`] when the reference is invalid or the
    /// secure credential file cannot be read, encoded, or written.
    pub fn store(
        &self,
        reference: &CredentialReference,
        secret: &Secret<String>,
    ) -> Result<StoredCredential, CredentialError> {
        reference.validate()?;

        if self.use_injected_store {
            match self.store_in_vault(reference.identifier(), secret) {
                Ok(()) => {
                    return Ok(StoredCredential {
                        source: CredentialSource::InjectedStore,
                        warnings: Vec::new(),
                    });
                }
                Err(VaultAccessError::Malformed) => {
                    return Err(CredentialError::MalformedCredentialStore);
                }
                Err(VaultAccessError::Encode) => {
                    return Err(CredentialError::EncodeCredentialStore);
                }
                Err(VaultAccessError::Unavailable) => {}
            }
        }

        write_credential_file_value(
            &self.credential_file_path,
            reference.identifier(),
            secret.expose_secret(),
        )?;
        Ok(StoredCredential {
            source: CredentialSource::CredentialFile(self.credential_file_path.clone()),
            warnings: self.file_warnings(reference.identifier()),
        })
    }

    fn file_warnings(&self, identifier: &str) -> Vec<CredentialWarning> {
        self.warn_on_credential_file
            .then(|| credential_file_warning(&self.credential_file_path, identifier))
            .into_iter()
            .collect()
    }

    fn resolve_from_vault(
        &self,
        identifier: &str,
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
        Ok(None)
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
        updated
            .credentials
            .insert(identifier.to_owned(), secret.expose_secret().clone());
        let encoded = encode_vault(&updated)?;
        if self
            .store
            .set(CREDENTIAL_VAULT_ID, &Secret::new(encoded))
            .is_err()
        {
            cache.writes_unavailable = true;
            return Err(VaultAccessError::Unavailable);
        }

        cache.vault = CachedVault::Loaded(updated);
        Ok(())
    }

    fn read_fresh_vault(&self) -> Result<Option<CredentialVault>, VaultAccessError> {
        match self.store.get_fresh(CREDENTIAL_VAULT_ID) {
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

        match self.store.get(CREDENTIAL_VAULT_ID) {
            Ok(Some(encoded)) => {
                let Ok(vault) = decode_vault(&encoded) else {
                    cache.vault = CachedVault::Malformed;
                    return Err(VaultAccessError::Malformed);
                };
                cache.vault = CachedVault::Loaded(vault);
            }
            Ok(None) => {
                cache.vault = CachedVault::Loaded(CredentialVault::default());
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
    if vault.version != CREDENTIAL_VAULT_VERSION
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

fn credential_file_warning(path: &Path, identifier: &str) -> CredentialWarning {
    tracing::warn!(
        credential_file_path = %path.display(),
        credential_reference = identifier,
        "injected credential store was unavailable; using the owner-private credential file"
    );
    CredentialWarning::OwnerPrivateCredentialFile {
        path: path.to_owned(),
    }
}

fn read_credential_file_value(
    path: &Path,
    identifier: &str,
) -> Result<Option<String>, CredentialError> {
    let Some(metadata) = credential_file_metadata(path)? else {
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

fn write_credential_file_value(
    path: &Path,
    identifier: &str,
    secret: &str,
) -> Result<(), CredentialError> {
    let parent = credential_file_parent(path);
    fs::create_dir_all(parent).map_err(|source| CredentialError::WriteFile {
        path: path.to_owned(),
        source,
    })?;

    // The lock must cover the fresh read, merge, temporary write, rename, and
    // directory sync. Otherwise two Rottweiler processes can each read the same
    // old document and silently discard the other's credential.
    let _write_lock = acquire_credential_file_write_lock(path)?;
    remove_stale_credential_temporary_files(path);
    let mut file = if credential_file_metadata(path)?.is_some() {
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

    let mut temporary_file = create_unique_credential_temporary_file(path)?;
    let temporary_path = temporary_file.path().to_owned();
    #[cfg(unix)]
    temporary_file
        .file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| CredentialError::WriteFile {
            path: temporary_path.clone(),
            source,
        })?;
    temporary_file
        .file_mut()
        .write_all(contents.as_bytes())
        .and_then(|()| temporary_file.file_mut().sync_all())
        .map_err(|source| CredentialError::WriteFile {
            path: temporary_path.clone(),
            source,
        })?;
    temporary_file.close();
    fs::rename(&temporary_path, path).map_err(|source| CredentialError::WriteFile {
        path: path.to_owned(),
        source,
    })?;
    sync_credential_parent_directory(parent, path)
}

fn read_document(path: &Path) -> Result<CredentialFile, CredentialError> {
    let metadata = credential_file_metadata(path)?.ok_or_else(|| CredentialError::ReadFile {
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

fn credential_file_metadata(path: &Path) -> Result<Option<fs::Metadata>, CredentialError> {
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

fn credential_file_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn credential_file_lock_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map_or_else(|| "credentials".into(), std::ffi::OsString::from);
    file_name.push(".lock");
    path.with_file_name(file_name)
}

fn remove_stale_credential_temporary_files(path: &Path) {
    let parent = credential_file_parent(path);
    let mut current_prefix = path
        .file_name()
        .map_or_else(|| "credentials".into(), std::ffi::OsString::from);
    current_prefix.push(".tmp.");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .as_encoded_bytes()
            .starts_with(current_prefix.as_encoded_bytes())
        {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() || file_type.is_symlink() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
fn acquire_credential_file_write_lock(path: &Path) -> Result<fs::File, CredentialError> {
    use rustix::fs::{Mode, OFlags};

    let lock_path = credential_file_lock_path(path);
    let owner = rustix::process::geteuid().as_raw();
    loop {
        match fs::symlink_metadata(&lock_path) {
            Ok(metadata) => validate_credential_lock_file(&lock_path, &metadata, owner)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match rustix::fs::open(
                    &lock_path,
                    OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                    Mode::RUSR | Mode::WUSR,
                ) {
                    Ok(descriptor) => {
                        let file = fs::File::from(descriptor);
                        file.set_permissions(fs::Permissions::from_mode(0o600))
                            .map_err(|source| CredentialError::WriteFile {
                                path: lock_path.clone(),
                                source,
                            })?;
                        validate_credential_lock_file(
                            &lock_path,
                            &file
                                .metadata()
                                .map_err(|source| CredentialError::WriteFile {
                                    path: lock_path.clone(),
                                    source,
                                })?,
                            owner,
                        )?;
                        file.lock().map_err(|source| CredentialError::WriteFile {
                            path: lock_path,
                            source,
                        })?;
                        return Ok(file);
                    }
                    Err(rustix::io::Errno::EXIST) => continue,
                    Err(source) => {
                        return Err(CredentialError::WriteFile {
                            path: lock_path,
                            source: std::io::Error::from(source),
                        });
                    }
                }
            }
            Err(source) => {
                return Err(CredentialError::WriteFile {
                    path: lock_path,
                    source,
                });
            }
        }

        let descriptor =
            rustix::fs::open(&lock_path, OFlags::RDWR | OFlags::NOFOLLOW, Mode::empty()).map_err(
                |source| CredentialError::WriteFile {
                    path: lock_path.clone(),
                    source: std::io::Error::from(source),
                },
            )?;
        let file = fs::File::from(descriptor);
        validate_credential_lock_file(
            &lock_path,
            &file
                .metadata()
                .map_err(|source| CredentialError::WriteFile {
                    path: lock_path.clone(),
                    source,
                })?,
            owner,
        )?;
        file.lock().map_err(|source| CredentialError::WriteFile {
            path: lock_path,
            source,
        })?;
        return Ok(file);
    }
}

#[cfg(unix)]
fn validate_credential_lock_file(
    path: &Path,
    metadata: &fs::Metadata,
    owner: u32,
) -> Result<(), CredentialError> {
    if !metadata.file_type().is_file() || metadata.uid() != owner {
        return Err(CredentialError::UnsafeFileType {
            path: path.to_owned(),
        });
    }
    validate_file_permissions(path, metadata)
}

#[cfg(not(unix))]
fn acquire_credential_file_write_lock(path: &Path) -> Result<fs::File, CredentialError> {
    let lock_path = credential_file_lock_path(path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|source| CredentialError::WriteFile {
            path: lock_path.clone(),
            source,
        })?;
    file.lock().map_err(|source| CredentialError::WriteFile {
        path: lock_path,
        source,
    })?;
    Ok(file)
}

struct CredentialTemporaryFile {
    path: PathBuf,
    file: Option<fs::File>,
}

impl CredentialTemporaryFile {
    fn path(&self) -> &Path {
        &self.path
    }

    fn file_mut(&mut self) -> &mut fs::File {
        match self.file.as_mut() {
            Some(file) => file,
            None => unreachable!("temporary credential file must be open"),
        }
    }

    fn close(&mut self) {
        drop(self.file.take());
    }
}

impl Drop for CredentialTemporaryFile {
    fn drop(&mut self) {
        // After a successful rename this is a harmless NotFound. After any
        // earlier failure it prevents this process from leaving sensitive data
        // behind. Cleanup errors cannot hide the original sanitized failure.
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn create_unique_credential_temporary_file(
    path: &Path,
) -> Result<CredentialTemporaryFile, CredentialError> {
    static NEXT_TEMPORARY_ID: AtomicU64 = AtomicU64::new(1);

    let base_name = path
        .file_name()
        .map_or_else(|| "credentials".into(), std::ffi::OsString::from);
    loop {
        let id = NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
        let mut file_name = base_name.clone();
        file_name.push(format!(".tmp.{}.{id}", std::process::id()));
        let temporary_path = path.with_file_name(file_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temporary_path) {
            Ok(file) => {
                return Ok(CredentialTemporaryFile {
                    path: temporary_path,
                    file: Some(file),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(CredentialError::WriteFile {
                    path: temporary_path,
                    source,
                });
            }
        }
    }
}

#[cfg(unix)]
fn sync_credential_parent_directory(parent: &Path, path: &Path) -> Result<(), CredentialError> {
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| CredentialError::WriteFile {
            path: path.to_owned(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_credential_parent_directory(_parent: &Path, _path: &Path) -> Result<(), CredentialError> {
    Ok(())
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
mod tests;
