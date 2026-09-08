//! Namespace laziness must preserve the ledger's authority over missing inventory.
use super::*;
use crate::checkpoint::CheckpointStore;
use tempfile::{TempDir, tempdir};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct NamespaceFixture {
    directory: TempDir,
    workspace: PathBuf,
    blobs: Arc<CheckpointBlobStore>,
}

impl NamespaceFixture {
    fn new() -> Result<Self, CheckpointError> {
        let directory = tempdir()?;
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace)?;
        let mut blobs = CheckpointBlobStore::open(directory.path(), &workspace)?;
        Arc::get_mut(&mut blobs)
            .ok_or(CheckpointError::CorruptBlobQuota)?
            .retained_bytes = 8;
        Ok(Self {
            directory,
            workspace,
            blobs,
        })
    }

    fn open(&self, name: &str) -> Result<CheckpointStore, CheckpointError> {
        CheckpointStore::open(
            &self.directory.path().join(name),
            &self.workspace,
            self.blobs.clone(),
        )
    }

    fn capture(
        &self,
        store: &CheckpointStore,
    ) -> Result<super::super::CheckpointManifest, CheckpointError> {
        fs::write(self.workspace.join("file"), b"original")?;
        store.checkpoint_known(
            "session",
            1,
            [PathBuf::from("file")],
            &mut CheckpointOperation::default(),
        )
    }
}

#[test]
fn empty_namespace_open_reopen_and_recovery_create_no_inventory() -> TestResult {
    let fixture = NamespaceFixture::new()?;
    for _ in 0..2 {
        let store = fixture.open("empty-fork")?;
        assert!(store.root.is_dir());
        assert_eq!(
            store.recover_opaque_mutations(&mut CheckpointOperation::default())?,
            0
        );
        assert!(store.recover_rewinds()?.is_empty());
        assert_eq!(fs::read_dir(&store.root)?.count(), 0);
        assert!(!fixture.blobs.root.exists());
    }
    // A different registered namespace does not grant this empty fork authority.
    fixture.capture(&fixture.open("active")?)?;
    let empty = fixture.open("empty-fork")?;
    assert!(empty.recover_rewinds()?.is_empty());
    assert_eq!(
        empty.recover_opaque_mutations(&mut CheckpointOperation::default())?,
        0
    );
    assert_eq!(fs::read_dir(&empty.root)?.count(), 0);
    let ledger = fixture.blobs.open_ledger()?;
    let count: u32 = ledger.query_row("SELECT count(*) FROM namespaces", [], |row| row.get(0))?;
    assert_eq!(count, 1);
    Ok(())
}

#[test]
fn first_capture_fork_and_rewind_survive_reopen() -> TestResult {
    let fixture = NamespaceFixture::new()?;
    let parent = fixture.open("parent")?;
    fixture.capture(&parent)?;
    let child = fixture.open("child")?;
    parent.fork_into(&child, "session", "child", None)?;
    super::validate_namespace_directories(&child.root)?;
    let handle = child.prepare_rewind("child", 0, "rewind-child")?;
    fs::write(fixture.workspace.join("file"), b"changed")?;
    drop(child);
    let reopened = fixture.open("child")?;
    let commits = reopened.recover_rewinds()?;
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].handle, handle);
    assert_eq!(fs::read(fixture.workspace.join("file"))?, b"original");
    reopened.acknowledge_rewind(&handle)?;
    assert!(fixture.open("child")?.recover_rewinds()?.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn readonly_namespace_lookup_accepts_parent_alias_but_rejects_ledger_symlink() -> TestResult {
    let fixture = NamespaceFixture::new()?;
    fixture.capture(&fixture.open("registered")?)?;
    let aliases = tempdir()?;
    let alias = aliases.path().join("storage");
    std::os::unix::fs::symlink(fixture.directory.path(), &alias)?;
    let blobs = CheckpointBlobStore::open(&alias, &fixture.workspace)?;
    let empty = CheckpointStore::open(&alias.join("empty"), &fixture.workspace, blobs.clone())?;
    assert!(empty.recover_rewinds()?.is_empty());
    assert_eq!(
        empty.recover_opaque_mutations(&mut CheckpointOperation::default())?,
        0
    );
    assert_eq!(fs::read_dir(&empty.root)?.count(), 0);

    let ledger = blobs.root.join("quota.sqlite");
    let retained = blobs.root.join("retained-quota.sqlite");
    fs::rename(&ledger, &retained)?;
    std::os::unix::fs::symlink(&retained, &ledger)?;
    assert!(empty.recover_rewinds().is_err());
    assert!(
        empty
            .recover_opaque_mutations(&mut CheckpointOperation::default())
            .is_err()
    );
    assert_eq!(fs::read_dir(&empty.root)?.count(), 0);
    assert!(fs::symlink_metadata(&ledger)?.file_type().is_symlink());
    let connection = Connection::open(retained)?;
    let count: u32 =
        connection.query_row("SELECT count(*) FROM namespaces", [], |row| row.get(0))?;
    assert_eq!(count, 1);
    Ok(())
}

#[test]
fn rewind_can_be_the_first_namespace_mutation() -> TestResult {
    let fixture = NamespaceFixture::new()?;
    let store = fixture.open("empty")?;
    let handle = store.prepare_rewind("session", 0, "empty-rewind")?;
    super::validate_namespace_directories(&store.root)?;
    drop(store);
    let reopened = fixture.open("empty")?;
    let commits = reopened.recover_rewinds()?;
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].handle, handle);
    reopened.acknowledge_rewind(&handle)?;
    Ok(())
}

#[test]
fn registered_missing_directory_refuses_recovery_capture_and_collection() -> TestResult {
    for name in NAMESPACE_DIRECTORIES {
        let fixture = NamespaceFixture::new()?;
        let damaged = fixture.open("damaged")?;
        let manifest = fixture.capture(&damaged)?;
        let crate::checkpoint::CheckpointFileState::Present { blob, bytes, .. } =
            &manifest.files["file"]
        else {
            return Err("capture did not retain its source".into());
        };
        damaged.prepare_rewind("session", 0, "prepared")?;
        let transaction =
            damaged.load_rewind_transaction("session", &mut CheckpointOperation::default())?;
        let review = damaged.load_review_ledger("session", &mut CheckpointOperation::default())?;
        fs::remove_dir_all(damaged.root.join(name))?;
        assert!(
            damaged.write_rewind_transaction(&transaction).is_err(),
            "{name}"
        );
        assert!(damaged.write_review_ledger(&review).is_err(), "{name}");
        let reopened = fixture.open("damaged")?;
        assert!(reopened.recover_rewinds().is_err(), "{name}");
        assert!(
            reopened
                .recover_opaque_mutations(&mut CheckpointOperation::default())
                .is_err(),
            "{name}"
        );
        assert!(fixture.capture(&reopened).is_err(), "{name}");
        let active = fixture.open("active")?;
        fs::write(fixture.workspace.join("file"), b"new-data")?;
        assert!(
            active
                .checkpoint_known(
                    "session",
                    2,
                    [PathBuf::from("file")],
                    &mut CheckpointOperation::default()
                )
                .is_err(),
            "{name}"
        );
        assert!(!reopened.root.join(name).exists(), "{name}");
        assert_eq!(active.read_valid_blob(blob, *bytes)?, b"original", "{name}");
    }
    Ok(())
}

#[test]
fn missing_lock_or_invalid_ledger_cannot_prove_an_empty_namespace() -> TestResult {
    for corruption in ["lock", "identity", "database", "missing"] {
        let fixture = NamespaceFixture::new()?;
        let store = fixture.open("registered")?;
        fixture.capture(&store)?;
        fs::remove_dir(store.root.join("pending"))?;
        match corruption {
            "lock" => fs::remove_file(fixture.blobs.root.join("writer.lock"))?,
            "identity" => {
                fixture
                    .blobs
                    .open_ledger()?
                    .execute("UPDATE quota SET lineage='foreign'", [])?;
            }
            "database" => fs::write(fixture.blobs.root.join("quota.sqlite"), b"invalid")?,
            "missing" => fs::remove_file(fixture.blobs.root.join("quota.sqlite"))?,
            _ => unreachable!(),
        }
        assert!(store.recover_rewinds().is_err(), "{corruption}");
        assert!(!store.root.join("pending").exists());
        if corruption == "lock" {
            assert!(!fixture.blobs.root.join("writer.lock").exists());
        }
    }
    Ok(())
}

#[test]
fn recovery_waits_for_first_registration_before_reading_durable_inventory() -> TestResult {
    let fixture = NamespaceFixture::new()?;
    let store = fixture.open("session")?;
    let mut operation = CheckpointOperation::default();
    let lock = fixture.blobs.lock_references(&mut operation)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let root = store.root.clone();
    let blobs = fixture.blobs.clone();
    let cancellation = operation.cancellation();
    let worker = std::thread::spawn(move || {
        let result = blobs
            .read_namespace_directory(&root, "pending", &mut operation)
            .map(|entries| entries.is_some());
        sender.send(result)
    });
    let waiting = receiver.recv_timeout(Duration::from_millis(30));
    let publication = (|| {
        let writer = BlobWriteGuard {
            owner: &fixture.blobs,
            connection: fixture.blobs.open_ledger()?,
            _lock: lock,
        };
        writer.register(&store.root)
    })();
    if publication.is_err() {
        cancellation.cancel();
    }
    let result = receiver.recv_timeout(Duration::from_secs(2));
    cancellation.cancel();
    // Settle the reader even when publication or an assertion fails.
    worker.join().map_err(|_| "reader panicked")??;
    publication?;
    assert!(matches!(
        waiting,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(result??);
    Ok(())
}

#[cfg(unix)]
#[test]
fn missing_inventory_rejects_special_or_oversized_authority_files() -> TestResult {
    for name in ["writer.lock", "quota.sqlite"] {
        for kind in ["directory", "symlink", "fifo", "oversized"] {
            if name == "writer.lock" && kind == "oversized" {
                continue;
            }
            let fixture = NamespaceFixture::new()?;
            let store = fixture.open("registered")?;
            fixture.capture(&store)?;
            fs::remove_dir(store.root.join("pending"))?;
            let path = fixture.blobs.root.join(name);
            fs::remove_file(&path)?;
            match kind {
                "directory" => fs::create_dir(&path)?,
                "symlink" => std::os::unix::fs::symlink(fixture.workspace.join("file"), &path)?,
                "fifo" => {
                    use std::os::unix::fs::FileTypeExt;
                    assert!(
                        std::process::Command::new("mkfifo")
                            .arg(&path)
                            .status()?
                            .success()
                    );
                    assert!(fs::symlink_metadata(&path)?.file_type().is_fifo());
                }
                "oversized" => File::create(&path)?.set_len(64 * 1024 * 1024 + 1)?,
                _ => unreachable!(),
            }
            assert!(store.recover_rewinds().is_err(), "{name}: {kind}");
            assert!(!store.root.join("pending").exists());
            assert!(fs::symlink_metadata(path).is_ok());
        }
    }
    Ok(())
}

#[test]
fn first_publication_between_missing_lock_and_ledger_probe_is_not_corruption() -> TestResult {
    let fixture = NamespaceFixture::new()?;
    let store = fixture.open("first")?;
    let mut observations = 0;
    let lock = fixture.blobs.existing_read_lock_using(
        &mut CheckpointOperation::default(),
        |path| {
            observations += 1;
            let observed = File::open(path);
            if observations == 1 {
                assert!(
                    matches!(&observed, Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                );
                // The syscall already observed absence; the first publisher
                // commits its namespace before that observation is consumed.
                let writer = fixture
                    .blobs
                    .reference_writer(&store.root, &mut CheckpointOperation::default())?;
                drop(writer);
            }
            Ok(observed?)
        },
    )?;
    assert!(lock.is_some());
    assert_eq!(observations, 2);
    drop(lock);
    assert!(store.recover_rewinds()?.is_empty());
    Ok(())
}

#[test]
fn malformed_namespace_view_cannot_run_unbounded_readonly_vm_work() -> TestResult {
    let fixture = NamespaceFixture::new()?;
    let store = fixture.open("registered")?;
    fixture.capture(&store)?;
    fs::remove_dir(store.root.join("pending"))?;
    let connection = fixture.blobs.open_ledger()?;
    connection.execute_batch(
        "ALTER TABLE namespaces RENAME TO retained_namespaces;
         CREATE VIEW namespaces AS WITH RECURSIVE spin(n) AS
             (VALUES(1) UNION ALL SELECT n+1 FROM spin)
             SELECT CAST(n AS TEXT) AS path FROM spin;",
    )?;
    drop(connection);
    assert!(matches!(
        store.recover_rewinds(),
        Err(CheckpointError::BlobLedger(error))
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
    ));
    assert!(!store.root.join("pending").exists());
    let connection = Connection::open(fixture.blobs.root.join("quota.sqlite"))?;
    let count: u32 =
        connection.query_row("SELECT count(*) FROM retained_namespaces", [], |row| {
            row.get(0)
        })?;
    assert_eq!(count, 1);
    Ok(())
}
