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
    CredentialStoreUnavailable, CredentialVault, NoExternalCredentialStore, Secret, decode_vault,
    encode_vault, read_document,
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
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
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
        let encoded = encode_vault(vault).unwrap_or_else(|_| panic!("test vault should encode"));
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
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
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
