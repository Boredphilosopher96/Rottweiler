use super::*;
use crate::checkpoint::{CheckpointFileState, CheckpointManifest, CheckpointStore};
use std::{io, path::PathBuf, sync::Barrier};
use tempfile::{TempDir, tempdir};
type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    root: TempDir,
    workspace: PathBuf,
    blobs: Arc<CheckpointBlobStore>,
}
impl Fixture {
    fn new(limit: u64) -> Result<Self, CheckpointError> {
        let root = tempdir()?;
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace)?;
        let mut blobs = CheckpointBlobStore::open(root.path(), &workspace)?;
        Arc::get_mut(&mut blobs)
            .ok_or(CheckpointError::CorruptBlobQuota)?
            .retained_bytes = limit;
        Ok(Self {
            root,
            workspace,
            blobs,
        })
    }
    fn store(&self, name: &str) -> Result<CheckpointStore, CheckpointError> {
        let mut owner = CheckpointBlobStore::open(self.root.path(), &self.workspace)?;
        Arc::get_mut(&mut owner)
            .ok_or(CheckpointError::CorruptBlobQuota)?
            .retained_bytes = self.blobs.retained_bytes;
        CheckpointStore::open(&self.root.path().join(name), &self.workspace, owner)
    }
    fn capture(
        &self,
        store: &CheckpointStore,
        turn: u64,
        bytes: &[u8],
    ) -> Result<CheckpointManifest, CheckpointError> {
        fs::write(self.workspace.join("file"), bytes)?;
        store.checkpoint_known(
            "session",
            turn,
            [PathBuf::from("file")],
            &mut CheckpointOperation::default(),
        )
    }
    fn used(&self) -> Result<u64, CheckpointError> {
        Ok(
            Connection::open(self.blobs.root.join("quota.sqlite"))?.query_row(
                "SELECT used_bytes FROM quota WHERE id=1",
                [],
                |row| row.get(0),
            )?,
        )
    }
}

#[test]
fn namespaces_share_quota_and_deduplicate_at_the_retained_limit() -> TestResult {
    let fixture = Fixture::new(8)?;
    let primary = fixture.store("primary/session-a/root-0")?;
    let additional = fixture.store("other-primary/session-b/root-1")?;
    assert!(
        !fixture.blobs.root.exists(),
        "opening does not initialize quota storage"
    );
    fixture.capture(&primary, 1, b"12345678")?;
    fixture.capture(&additional, 1, b"12345678")?;
    assert_eq!(fixture.used()?, 8);
    assert!(matches!(
        fixture.capture(&additional, 2, b"new"),
        Err(CheckpointError::BlobQuotaExceeded)
    ));
    assert!(!additional.manifest_path("session", 2).exists());
    assert_eq!(fs::read(fixture.workspace.join("file"))?, b"new");
    let original = primary.load_manifest("session", 1)?;
    let CheckpointFileState::Present { blob, bytes, .. } = &original.files["file"] else {
        return Err("missing source".into());
    };
    assert_eq!(primary.read_valid_blob(blob, *bytes)?, b"12345678");
    Ok(())
}

#[test]
fn physical_workspace_alias_binds_the_same_authority() -> TestResult {
    let fixture = Fixture::new(8)?;
    #[cfg(unix)]
    {
        let alias = fixture.root.path().join("alias");
        std::os::unix::fs::symlink(&fixture.workspace, &alias)?;
        let owner = CheckpointBlobStore::open(fixture.root.path(), &alias)?;
        assert_eq!(owner.root, fixture.blobs.root);
    }
    let other = fixture.root.path().join("different");
    fs::create_dir(&other)?;
    let owner = CheckpointBlobStore::open(fixture.root.path(), &other)?;
    assert_ne!(owner.root, fixture.blobs.root);
    assert!(matches!(
        CheckpointStore::open(
            &fixture.root.path().join("wrong"),
            &other,
            fixture.blobs.clone()
        ),
        Err(CheckpointError::BlobWorkspaceMismatch)
    ));
    Ok(())
}

#[test]
fn aborted_publication_is_reconciled_without_reclaiming_a_live_manifest() -> TestResult {
    let fixture = Fixture::new(8)?;
    let store = fixture.store("session")?;
    fixture.capture(&store, 1, b"live")?;
    let mut operation = CheckpointOperation::default();
    let mut writer = fixture.blobs.begin(&store.root, &mut operation)?;
    let orphan = writer.capture(&mut b"lost".as_slice(), None, &mut operation)?;
    // Dropping the writer before manifest publication simulates the crash cut.
    drop(writer);
    assert_eq!(fixture.used()?, 8);
    fixture.capture(&store, 2, b"next")?;
    assert_eq!(fixture.used()?, 8);
    let CheckpointFileState::Present { blob, bytes, .. } = orphan else {
        return Err("missing orphan".into());
    };
    assert!(store.read_valid_blob(&blob, bytes).is_err());
    assert_eq!(store.load_manifest("session", 1)?.files.len(), 1);
    Ok(())
}

#[test]
fn quota_pressure_collects_only_unreferenced_content() -> TestResult {
    let fixture = Fixture::new(8)?;
    let store = fixture.store("session")?;
    let mut operation = CheckpointOperation::default();
    let mut writer = fixture.blobs.begin(&store.root, &mut operation)?;
    writer.capture(&mut b"orphaned".as_slice(), None, &mut operation)?;
    writer.finish()?; // No published manifest refers to this admitted content.
    fixture.capture(&store, 1, b"new-data")?;
    assert_eq!(fixture.used()?, 8);
    Ok(())
}

#[test]
fn malformed_reference_inventory_prevents_any_blob_reclamation() -> TestResult {
    let fixture = Fixture::new(8)?;
    let store = fixture.store("session")?;
    let manifest = fixture.capture(&store, 1, b"original")?;
    let CheckpointFileState::Present { blob, .. } = &manifest.files["file"] else {
        return Err("missing source".into());
    };
    let path = fixture.blobs.directory().join(&blob[..2]).join(blob);
    fs::write(store.manifest_path("session", 1), b"{\"incomplete\":")?;
    assert!(fixture.capture(&store, 2, b"changed!").is_err());
    assert_eq!(fs::read(path)?, b"original");
    Ok(())
}

#[test]
fn fork_and_prepared_rewind_keep_their_blob_references_live() -> TestResult {
    let fixture = Fixture::new(8)?;
    let parent = fixture.store("parent")?;
    let child = fixture.store("child")?;
    let manifest = fixture.capture(&parent, 1, b"original")?;
    parent.fork_into(&child, "session", "child", None)?;
    fs::remove_file(parent.manifest_path("session", 1))?;
    assert!(matches!(
        fixture.capture(&parent, 2, b"changed!"),
        Err(CheckpointError::BlobQuotaExceeded)
    ));
    child.prepare_rewind("child", 0, "quota-rewind")?;
    fs::remove_file(child.manifest_path("child", 1))?;
    assert!(matches!(
        fixture.capture(&parent, 2, b"changed!"),
        Err(CheckpointError::BlobQuotaExceeded)
    ));
    let CheckpointFileState::Present { blob, bytes, .. } = &manifest.files["file"] else {
        return Err("missing source".into());
    };
    assert_eq!(child.read_valid_blob(blob, *bytes)?, b"original");
    Ok(())
}

#[test]
fn concurrent_writers_cannot_oversubscribe_shared_content() -> TestResult {
    let fixture = Fixture::new(8)?;
    let first = fixture.store("first")?;
    let second = fixture.store("second")?;
    fs::write(fixture.workspace.join("a"), b"aaaaaaaa")?;
    fs::write(fixture.workspace.join("b"), b"bbbbbbbb")?;
    let barrier = Arc::new(Barrier::new(2));
    let workers = [(first, "a"), (second, "b")].map(|(store, path)| {
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            store.checkpoint_known(
                "session",
                1,
                [PathBuf::from(path)],
                &mut CheckpointOperation::default(),
            )
        })
    });
    let mut success = 0;
    let mut refused = 0;
    for worker in workers {
        match worker
            .join()
            .map_err(|_| io::Error::other("worker panicked"))?
        {
            Ok(_) => success += 1,
            Err(CheckpointError::BlobQuotaExceeded) => refused += 1,
            Err(error) => return Err(error.into()),
        }
    }
    assert_eq!((success, refused), (1, 1));
    assert_eq!(fixture.used()?, 8);
    Ok(())
}

#[test]
fn incomplete_ledger_cannot_adopt_unregistered_existing_blobs() -> TestResult {
    let fixture = Fixture::new(8)?;
    let store = fixture.store("session")?;
    fixture.capture(&store, 1, b"original")?;
    fs::remove_file(fixture.blobs.root.join("quota.sqlite"))?;
    assert!(matches!(
        fixture.capture(&store, 2, b"changed!"),
        Err(CheckpointError::CorruptBlobQuota)
    ));
    assert!(store.load_manifest("session", 1).is_ok());
    Ok(())
}

#[test]
fn protected_unpublished_files_survive_collection_between_captures() -> TestResult {
    let fixture = Fixture::new(8)?;
    let store = fixture.store("session")?;
    let mut operation = CheckpointOperation::default();
    let mut writer = fixture.blobs.begin(&store.root, &mut operation)?;
    let state = writer.capture(&mut b"12345678".as_slice(), None, &mut operation)?;
    assert!(matches!(
        writer.capture(&mut b"new".as_slice(), None, &mut operation),
        Err(CheckpointError::BlobQuotaExceeded)
    ));
    let CheckpointFileState::Present { blob, bytes, .. } = &state else {
        return Err("missing source".into());
    };
    assert_eq!(store.read_valid_blob(blob, *bytes)?, b"12345678");
    // This operation did not publish and cannot declare successful completion.
    drop(writer);
    fixture.capture(&store, 1, b"new")?;
    assert_eq!(fixture.used()?, 3);
    Ok(())
}

#[test]
fn abandoned_staging_credit_is_released_only_after_exclusive_reconciliation() -> TestResult {
    let fixture = Fixture::new(8)?;
    let store = fixture.store("session")?;
    let mut operation = CheckpointOperation::default();
    let writer = fixture.blobs.begin(&store.root, &mut operation)?;
    writer.connection.execute(
        "UPDATE quota SET staged=?1 WHERE id=1",
        [MAX_CAPTURE_FILE_BYTES],
    )?;
    fs::write(
        fixture.blobs.root.join("staging/capture-abandoned"),
        b"partial",
    )?;
    drop(writer);
    fixture.capture(&store, 1, b"new")?;
    let staged: u64 = Connection::open(fixture.blobs.root.join("quota.sqlite"))?.query_row(
        "SELECT staged FROM quota WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(staged, 0);
    assert_eq!(fs::read_dir(fixture.blobs.root.join("staging"))?.count(), 0);
    assert_eq!(fixture.used()?, 3);
    Ok(())
}

#[test]
fn read_open_never_erases_an_active_writers_unpublished_manifest() -> TestResult {
    let fixture = Fixture::new(8)?;
    let store = fixture.store("session")?;
    let mut operation = CheckpointOperation::default();
    let writer = fixture.blobs.begin(&store.root, &mut operation)?;
    let session = store.root.join("manifests/session");
    fs::create_dir_all(&session)?;
    let temporary = session.join(".rw-123-1.tmp");
    fs::write(&temporary, b"in progress")?;
    let reopened = fixture.store("session")?;
    assert!(reopened.manifest_turns("session")?.is_empty());
    assert!(temporary.exists());
    drop(writer);
    fixture.capture(&reopened, 1, b"new")?;
    assert!(!temporary.exists());
    Ok(())
}

#[test]
fn unregistered_local_blob_layout_is_rejected_without_deleting_it() -> TestResult {
    let fixture = Fixture::new(8)?;
    let old = fixture.root.path().join("session/checkpoints/blobs");
    fs::create_dir_all(&old)?;
    fs::write(old.join("retained"), b"source")?;
    assert!(matches!(
        fixture.store("session"),
        Err(CheckpointError::LegacyBlobLayout)
    ));
    assert_eq!(fs::read(old.join("retained"))?, b"source");
    Ok(())
}
