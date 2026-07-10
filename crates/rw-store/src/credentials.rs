//! Provider-blind credential lookup and storage.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const KEYCHAIN_SERVICE: &str = "dev.rottweiler.credentials";
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
    /// Reads a credential, returning `None` when no entry exists.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainUnavailable`] without exposing backend diagnostics.
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable>;

    /// Creates or replaces a credential.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainUnavailable`] without exposing backend diagnostics.
    fn set(&self, identifier: &str, secret: &Secret<String>) -> Result<(), KeychainUnavailable>;
}

/// Operating-system keychain backed by the current `keyring` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeychain;

impl CredentialKeychain for OsKeychain {
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        let entry =
            keyring::Entry::new(KEYCHAIN_SERVICE, identifier).map_err(|_| KeychainUnavailable)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(Secret::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(KeychainUnavailable),
        }
    }

    fn set(&self, identifier: &str, secret: &Secret<String>) -> Result<(), KeychainUnavailable> {
        let entry =
            keyring::Entry::new(KEYCHAIN_SERVICE, identifier).map_err(|_| KeychainUnavailable)?;
        entry
            .set_password(secret.expose_secret())
            .map_err(|_| KeychainUnavailable)
    }
}

/// Credential manager with injectable environment and keychain boundaries.
pub struct CredentialManager<E = SystemEnvironment, K = OsKeychain> {
    environment: E,
    keychain: K,
    fallback_path: PathBuf,
}

impl CredentialManager<SystemEnvironment, OsKeychain> {
    /// Creates the production manager using the process environment and OS keychain.
    #[must_use]
    pub fn system(fallback_path: impl Into<PathBuf>) -> Self {
        Self::with_backends(SystemEnvironment, OsKeychain, fallback_path)
    }
}

impl<E, K> CredentialManager<E, K>
where
    E: CredentialEnvironment,
    K: CredentialKeychain,
{
    /// Creates a manager with deterministic/injectable external boundaries.
    #[must_use]
    pub fn with_backends(environment: E, keychain: K, fallback_path: impl Into<PathBuf>) -> Self {
        Self {
            environment,
            keychain,
            fallback_path: fallback_path.into(),
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

        let keychain_unavailable = match self.keychain.get(reference.identifier()) {
            Ok(Some(secret)) => {
                return Ok(ResolvedCredential {
                    secret,
                    source: CredentialSource::OsKeychain,
                    warnings: Vec::new(),
                });
            }
            Ok(None) => false,
            Err(_) => true,
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

        if self.keychain.set(reference.identifier(), secret).is_ok() {
            return Ok(StoredCredential {
                source: CredentialSource::OsKeychain,
                warnings: Vec::new(),
            });
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
    use std::sync::Mutex;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::{
        CredentialEnvironment, CredentialError, CredentialKeychain, CredentialManager,
        CredentialReference, CredentialSource, KeychainUnavailable, Secret,
    };

    #[derive(Debug, Default)]
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
