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
    /// credential-file read. This path never performs migration or any write.
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
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::{
        CREDENTIAL_VAULT_ID, CredentialEnvironment, CredentialError, CredentialInventoryItem,
        CredentialManager, CredentialReference, CredentialSource, CredentialStore,
        CredentialStoreUnavailable, CredentialVault, NoExternalCredentialStore, Secret,
        decode_vault, encode_vault, read_document,
    };

    const SUBPROCESS_CREDENTIAL_PATH: &str = "RW_TEST_SUBPROCESS_CREDENTIAL_PATH";
    const SUBPROCESS_CREDENTIAL_IDENTIFIER: &str = "RW_TEST_SUBPROCESS_CREDENTIAL_IDENTIFIER";
    const SUBPROCESS_CREDENTIAL_START: &str = "RW_TEST_SUBPROCESS_CREDENTIAL_START";

    #[derive(Debug, Default, Clone)]
    struct TestEnvironment(BTreeMap<String, String>);

    impl CredentialEnvironment for TestEnvironment {
        fn get(&self, name: &str) -> Result<Option<String>, CredentialError> {
            Ok(self.0.get(name).cloned())
        }
    }

    #[derive(Debug, Default)]
    struct TestCredentialStore {
        values: Mutex<BTreeMap<String, String>>,
        unavailable: bool,
    }

    impl TestCredentialStore {
        fn unavailable() -> Self {
            Self {
                values: Mutex::new(BTreeMap::new()),
                unavailable: true,
            }
        }
    }

    impl CredentialStore for TestCredentialStore {
        fn get(
            &self,
            identifier: &str,
        ) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
            if self.unavailable {
                return Err(CredentialStoreUnavailable);
            }
            let values = self.values.lock().map_err(|_| CredentialStoreUnavailable)?;
            Ok(values.get(identifier).cloned().map(Secret::new))
        }

        fn set(
            &self,
            identifier: &str,
            secret: &Secret<String>,
        ) -> Result<(), CredentialStoreUnavailable> {
            if self.unavailable {
                return Err(CredentialStoreUnavailable);
            }
            let mut values = self.values.lock().map_err(|_| CredentialStoreUnavailable)?;
            values.insert(identifier.to_owned(), secret.expose_secret().clone());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingCredentialStore(Arc<Mutex<RecordingCredentialStoreState>>);

    #[derive(Default)]
    struct RecordingCredentialStoreState {
        vault: Option<String>,
        calls: Vec<String>,
        vault_get_unavailable: bool,
        vault_set_unavailable: bool,
    }

    impl RecordingCredentialStore {
        fn with_vault(vault: &CredentialVault) -> Self {
            let encoded =
                encode_vault(vault).unwrap_or_else(|_| panic!("test vault should encode"));
            let store = Self::default();
            store.0.lock().expect("recording store should lock").vault = Some(encoded);
            store
        }

        fn calls(&self) -> Vec<String> {
            self.0
                .lock()
                .expect("recording store should lock")
                .calls
                .clone()
        }

        fn decoded_vault(&self) -> Option<CredentialVault> {
            self.0
                .lock()
                .expect("recording store should lock")
                .vault
                .as_ref()
                .map(|encoded| {
                    decode_vault(&Secret::new(encoded.clone()))
                        .expect("recorded test vault should decode")
                })
        }
    }

    impl CredentialStore for RecordingCredentialStore {
        fn get(
            &self,
            identifier: &str,
        ) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
            let mut state = self.0.lock().map_err(|_| CredentialStoreUnavailable)?;
            state.calls.push(format!("get:{identifier}"));
            if state.vault_get_unavailable {
                return Err(CredentialStoreUnavailable);
            }
            Ok(state.vault.clone().map(Secret::new))
        }

        fn set(
            &self,
            identifier: &str,
            secret: &Secret<String>,
        ) -> Result<(), CredentialStoreUnavailable> {
            let mut state = self.0.lock().map_err(|_| CredentialStoreUnavailable)?;
            state.calls.push(format!("set:{identifier}"));
            if state.vault_set_unavailable {
                return Err(CredentialStoreUnavailable);
            }
            state.vault = Some(secret.expose_secret().clone());
            Ok(())
        }

        fn get_authorized(
            &self,
            identifier: &str,
        ) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
            let mut state = self.0.lock().map_err(|_| CredentialStoreUnavailable)?;
            state.calls.push(format!("get-authorized:{identifier}"));
            Ok(state.vault.clone().map(Secret::new))
        }

        fn get_fresh(
            &self,
            identifier: &str,
        ) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
            let mut state = self.0.lock().map_err(|_| CredentialStoreUnavailable)?;
            state.calls.push(format!("get-fresh:{identifier}"));
            if state.vault_get_unavailable {
                return Err(CredentialStoreUnavailable);
            }
            Ok(state.vault.clone().map(Secret::new))
        }
    }

    #[test]
    fn empty_inventory_skips_store_and_file_access() {
        let root = tempdir().expect("temporary root should be created");
        let store = RecordingCredentialStore::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            // A directory is intentionally unsafe as a credential file. Success
            // therefore proves the credential-file metadata/document path was skipped.
            root.path(),
        );
        let inventory = manager
            .resolve_inventory(&[])
            .expect("empty inventory should be side-effect free");
        assert!(inventory.is_empty());
        assert!(store.calls().is_empty());
    }

    #[test]
    fn environment_satisfied_inventory_skips_store_and_file_access() {
        let root = tempdir().expect("temporary root should be created");
        let store = RecordingCredentialStore::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment(BTreeMap::from([(
                "RW_INVENTORY_TOKEN".to_owned(),
                "environment-token".to_owned(),
            )])),
            store.clone(),
            // As above, touching this directory as a credential file would fail.
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
        assert!(store.calls().is_empty());
    }

    #[test]
    fn missing_inventory_reads_one_store_document_without_writes() {
        let root = tempdir().expect("temporary root should be created");
        let store = RecordingCredentialStore::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
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
        assert_eq!(store.calls(), vec![format!("get:{CREDENTIAL_VAULT_ID}")]);
    }

    #[test]
    fn authorized_resolution_retries_once_after_passive_store_failure() {
        let root = tempdir().expect("temporary root should be created");
        let mut vault = CredentialVault::default();
        vault
            .credentials
            .insert("first".to_owned(), "secret-one".to_owned());
        vault
            .credentials
            .insert("second".to_owned(), "secret-two".to_owned());
        let store = RecordingCredentialStore::with_vault(&vault);
        store
            .0
            .lock()
            .expect("recording store should lock")
            .vault_get_unavailable = true;
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            root.path().join("credentials.toml"),
        );

        assert!(matches!(
            manager.resolve(&CredentialReference::new("first")),
            Err(CredentialError::CredentialStoreUnavailable { .. })
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
            store.calls(),
            vec![
                format!("get:{CREDENTIAL_VAULT_ID}"),
                format!("get-authorized:{CREDENTIAL_VAULT_ID}"),
            ]
        );
    }

    #[test]
    fn authorized_resolution_reports_missing_without_migration() {
        let root = tempdir().expect("temporary root should be created");
        let store = RecordingCredentialStore::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            root.path().join("credentials.toml"),
        );

        let error = manager
            .resolve_authorized(&CredentialReference::new("provider-token"))
            .expect_err("missing credentials are not synthesized");

        assert!(matches!(error, CredentialError::NotFound { .. }));
        assert_eq!(
            store.calls(),
            vec![format!("get-authorized:{CREDENTIAL_VAULT_ID}")]
        );
    }

    #[test]
    fn production_manager_always_persists_to_the_owner_private_file() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("private").join("credentials.toml");
        let reference = CredentialReference::new("provider-token");
        let manager = CredentialManager::system(path.clone());

        let stored = manager
            .store(&reference, &Secret::new("persisted-secret".to_owned()))
            .expect("production credential should store");
        assert_eq!(
            stored.source(),
            &CredentialSource::CredentialFile(path.clone())
        );
        assert!(stored.warnings().is_empty());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path)
                .expect("credential file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let reloaded = CredentialManager::system(path)
            .resolve_authorized(&reference)
            .expect("production credential should survive restart");
        assert_eq!(reloaded.secret().expose_secret(), "persisted-secret");
        assert!(reloaded.warnings().is_empty());
        assert!(NoExternalCredentialStore.get(CREDENTIAL_VAULT_ID).is_ok());
        assert!(
            NoExternalCredentialStore
                .set(
                    CREDENTIAL_VAULT_ID,
                    &Secret::new("never-written".to_owned())
                )
                .is_err()
        );
    }

    #[test]
    fn credential_file_subprocess_writer() {
        let Some(path) = std::env::var_os(SUBPROCESS_CREDENTIAL_PATH) else {
            return;
        };
        let identifier = std::env::var(SUBPROCESS_CREDENTIAL_IDENTIFIER)
            .expect("subprocess credential identifier should be configured");
        let start = std::env::var_os(SUBPROCESS_CREDENTIAL_START)
            .expect("subprocess start marker should be configured");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !std::path::Path::new(&start).exists() {
            assert!(
                Instant::now() < deadline,
                "subprocess start marker should appear"
            );
            thread::sleep(Duration::from_millis(5));
        }

        CredentialManager::system(path)
            .store(
                &CredentialReference::new(&identifier),
                &Secret::new(format!("secret-{identifier}")),
            )
            .expect("subprocess credential writer should succeed");
    }

    #[test]
    fn concurrent_process_credential_file_writers_preserve_every_value() {
        const WRITERS: usize = 8;

        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("private/credentials.toml");
        let start = root.path().join("start");
        let executable = std::env::current_exe().expect("test executable should be available");
        let mut children = (0..WRITERS)
            .map(|writer| {
                Command::new(&executable)
                    .arg("--exact")
                    .arg("credentials::tests::credential_file_subprocess_writer")
                    .arg("--nocapture")
                    .env(SUBPROCESS_CREDENTIAL_PATH, &path)
                    .env(
                        SUBPROCESS_CREDENTIAL_IDENTIFIER,
                        format!("provider-{writer}"),
                    )
                    .env(SUBPROCESS_CREDENTIAL_START, &start)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("credential writer subprocess should start")
            })
            .collect::<Vec<_>>();
        fs::write(&start, b"go").expect("start marker should be written");

        for child in children.drain(..) {
            let output = child
                .wait_with_output()
                .expect("credential writer subprocess should finish");
            assert!(
                output.status.success(),
                "credential writer failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let document = read_document(&path).expect("merged credential document should decode");
        assert_eq!(document.credentials.len(), WRITERS);
        for writer in 0..WRITERS {
            let identifier = format!("provider-{writer}");
            assert_eq!(
                document.credentials.get(&identifier),
                Some(&format!("secret-{identifier}"))
            );
        }
    }

    #[test]
    fn stale_credential_temporary_files_do_not_block_a_later_write() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("credentials.toml");
        let stale = root.path().join("credentials.toml.tmp.1234.9");
        fs::write(&stale, b"stale-secret-data").expect("stale temporary file should be written");

        CredentialManager::system(path.clone())
            .store(
                &CredentialReference::new("provider-token"),
                &Secret::new("fresh-secret".to_owned()),
            )
            .expect("stale temporary files must not block credential storage");

        assert!(!stale.exists());
        let document = read_document(&path).expect("credential document should decode");
        assert_eq!(
            document.credentials.get("provider-token"),
            Some(&"fresh-secret".to_owned())
        );
    }

    #[test]
    fn environment_wins_over_store_and_file() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("credentials.toml");
        let environment = TestEnvironment(BTreeMap::from([(
            "RW_TEST_TOKEN".to_owned(),
            "from-environment".to_owned(),
        )]));
        let store = TestCredentialStore::default();
        store
            .set("primary", &Secret::new("from-store".to_owned()))
            .expect("test store should accept a value");
        let manager = CredentialManager::with_backends(environment, store, path);
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
    fn store_wins_when_environment_is_absent() {
        let root = tempdir().expect("temporary directory should be created");
        let store = TestCredentialStore::default();
        let mut vault = CredentialVault::default();
        vault
            .credentials
            .insert("primary".to_owned(), "from-store".to_owned());
        store
            .set(
                CREDENTIAL_VAULT_ID,
                &Secret::new(encode_vault(&vault).expect("vault encoding")),
            )
            .expect("test store should accept a value");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store,
            root.path().join("credentials.toml"),
        );

        let resolved = manager
            .resolve(&CredentialReference::new("primary"))
            .expect("store credential should resolve");

        assert_eq!(resolved.secret().expose_secret(), "from-store");
        assert_eq!(resolved.source(), &CredentialSource::InjectedStore);
        assert!(resolved.warnings().is_empty());
    }

    #[test]
    fn all_logical_credentials_share_one_cached_vault_item() {
        let root = tempdir().expect("temporary directory should be created");
        let store = RecordingCredentialStore::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            root.path().join("credentials.toml"),
        );

        for (identifier, value) in [("provider-a", "secret-a"), ("proxy-b", "secret-b")] {
            let stored = manager
                .store(
                    &CredentialReference::new(identifier),
                    &Secret::new(value.to_owned()),
                )
                .expect("vault credential should store");
            assert_eq!(stored.source(), &CredentialSource::InjectedStore);
        }

        for (identifier, value) in [("provider-a", "secret-a"), ("proxy-b", "secret-b")] {
            let resolved = manager
                .resolve(&CredentialReference::new(identifier))
                .expect("cached vault credential should resolve");
            assert_eq!(resolved.secret().expose_secret(), value);
        }

        assert_eq!(
            store.calls(),
            vec![
                format!("get-fresh:{CREDENTIAL_VAULT_ID}"),
                format!("set:{CREDENTIAL_VAULT_ID}"),
                format!("get-fresh:{CREDENTIAL_VAULT_ID}"),
                format!("set:{CREDENTIAL_VAULT_ID}"),
            ]
        );
        let vault = store
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
        let store = RecordingCredentialStore::with_vault(&initial);
        let first = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            root.path().join("first/credentials.toml"),
        );
        let second = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            root.path().join("second/other-credentials.toml"),
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

        let vault = store
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
            store.calls(),
            vec![
                format!("get:{CREDENTIAL_VAULT_ID}"),
                format!("get:{CREDENTIAL_VAULT_ID}"),
                format!("get-fresh:{CREDENTIAL_VAULT_ID}"),
                format!("set:{CREDENTIAL_VAULT_ID}"),
                format!("get-fresh:{CREDENTIAL_VAULT_ID}"),
                format!("set:{CREDENTIAL_VAULT_ID}"),
            ]
        );
    }

    #[test]
    fn existing_store_is_read_once_for_missing_ids() {
        let root = tempdir().expect("temporary directory should be created");
        let vault = CredentialVault {
            credentials: BTreeMap::from([
                ("provider-a".to_owned(), "secret-a".to_owned()),
                ("proxy-b".to_owned(), "secret-b".to_owned()),
            ]),
            ..CredentialVault::default()
        };
        let store = RecordingCredentialStore::with_vault(&vault);
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
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

        assert_eq!(store.calls(), vec![format!("get:{CREDENTIAL_VAULT_ID}")]);
    }

    #[test]
    fn missing_store_never_triggers_writes() {
        let root = tempdir().expect("temporary directory should be created");
        let store = RecordingCredentialStore::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            root.path().join("credentials.toml"),
        );

        for identifier in ["first", "second"] {
            assert!(matches!(
                manager.resolve(&CredentialReference::new(identifier)),
                Err(CredentialError::NotFound { .. })
            ));
        }
        assert_eq!(store.calls(), vec![format!("get:{CREDENTIAL_VAULT_ID}")]);
        assert!(store.decoded_vault().is_none());
    }

    #[test]
    fn malformed_vault_is_sanitized_and_never_overwritten_or_bypassed() {
        const CANARY: &str = "rw-malformed-vault-canary";
        let root = tempdir().expect("temporary directory should be created");
        let credential_file_path = root.path().join("credentials.toml");
        super::write_credential_file_value(
            &credential_file_path,
            "primary",
            "credential-file-must-not-win",
        )
        .expect("credential-file fixture should be written");
        let store = RecordingCredentialStore::default();
        store.0.lock().expect("recording store should lock").vault =
            Some(format!("malformed {CANARY}"));
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            credential_file_path,
        );

        let resolve_error = manager
            .resolve(&CredentialReference::new("primary"))
            .expect_err("malformed vault must fail closed before credential-file access");
        assert!(matches!(
            resolve_error,
            CredentialError::MalformedCredentialStore
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
            CredentialError::MalformedCredentialStore
        ));
        assert_eq!(store.calls(), vec![format!("get:{CREDENTIAL_VAULT_ID}")]);
    }

    #[test]
    fn unavailable_store_access_is_cached_for_the_manager_lifetime() {
        let root = tempdir().expect("temporary directory should be created");
        let store = RecordingCredentialStore::default();
        store
            .0
            .lock()
            .expect("recording store should lock")
            .vault_get_unavailable = true;
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            root.path().join("credentials.toml"),
        );

        for identifier in ["provider-a", "proxy-b", "provider-c"] {
            assert!(matches!(
                manager.resolve(&CredentialReference::new(identifier)),
                Err(CredentialError::CredentialStoreUnavailable { .. })
            ));
        }

        assert_eq!(store.calls(), vec![format!("get:{CREDENTIAL_VAULT_ID}")]);
    }

    #[test]
    fn explicit_store_retries_after_a_passive_store_read_was_unavailable() {
        let root = tempdir().expect("temporary directory should be created");
        let store = RecordingCredentialStore::default();
        store
            .0
            .lock()
            .expect("recording store should lock")
            .vault_get_unavailable = true;
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            store.clone(),
            root.path().join("credentials.toml"),
        );

        assert!(matches!(
            manager.resolve(&CredentialReference::new("provider-a")),
            Err(CredentialError::CredentialStoreUnavailable { .. })
        ));
        store
            .0
            .lock()
            .expect("recording store should lock")
            .vault_get_unavailable = false;

        let stored = manager
            .store(
                &CredentialReference::new("provider-a"),
                &Secret::new("replacement".to_owned()),
            )
            .expect("an explicit store should retry the secure vault");

        assert_eq!(stored.source(), &CredentialSource::InjectedStore);
        assert_eq!(
            store.calls(),
            vec![
                format!("get:{CREDENTIAL_VAULT_ID}"),
                format!("get-fresh:{CREDENTIAL_VAULT_ID}"),
                format!("set:{CREDENTIAL_VAULT_ID}"),
            ]
        );
    }

    #[test]
    fn unavailable_injected_store_uses_mode_0600_file() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("private").join("credentials.toml");
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            TestCredentialStore::unavailable(),
            path.clone(),
        );
        let reference = CredentialReference::new("primary");

        let stored = manager
            .store(&reference, &Secret::new("file-secret".to_owned()))
            .expect("credential-file value should be stored");
        assert_eq!(
            stored.source(),
            &CredentialSource::CredentialFile(path.clone())
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
            .expect("credential-file value should resolve");
        assert_eq!(resolved.secret().expose_secret(), "file-secret");
        assert_eq!(resolved.warnings().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn insecure_credential_file_permissions_fail_closed() {
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
            TestCredentialStore::unavailable(),
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
            TestCredentialStore::unavailable(),
            path,
        );
        let error = manager
            .resolve(&CredentialReference::new("primary"))
            .expect_err("malformed credential file must fail");
        assert!(!format!("{error:?}").contains(CANARY));
        assert!(!error.to_string().contains(CANARY));
    }
}
