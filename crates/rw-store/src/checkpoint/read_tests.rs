use super::*;
use tempfile::{TempDir, tempdir};
type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    _root: TempDir,
    store: CheckpointStore,
}
impl Fixture {
    fn new() -> Result<Self, CheckpointError> {
        let root = tempdir()?;
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace)?;
        let blobs = CheckpointBlobStore::open(root.path(), &workspace)?;
        let store = CheckpointStore::open(&root.path().join("session"), &workspace, blobs)?;
        Ok(Self { _root: root, store })
    }
    fn manifest(&self, turn: u64, paths: &[&str]) -> Result<(), CheckpointError> {
        self.store.persist_manifest(
            "session",
            turn,
            paths
                .iter()
                .map(|path| {
                    (
                        (*path).to_owned(),
                        CheckpointFileState::Unrestorable {
                            reason: format!("turn {turn}"),
                        },
                    )
                })
                .collect(),
        )?;
        Ok(())
    }
}

#[test]
fn turn_collection_rejects_before_decoding_any_manifest() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.manifest(1, &["a"])?;
    fixture.manifest(2, &["a"])?;
    // Both files are intentionally invalid: collection admission must happen
    // before either source can reach the decoder.
    for turn in [1, 2] {
        fs::write(fixture.store.manifest_path("session", turn), b"not JSON")?;
    }
    let mut operation = CheckpointOperation::default().read_limits(1, 1024 * 1024, 1024 * 1024);
    assert!(matches!(
        fixture
            .store
            .build_rewind_steps("session", 0, &mut operation),
        Err(CheckpointError::OperationLimit(_))
    ));
    assert!(!fixture.store.rewind_path("session").exists());
    Ok(())
}

#[test]
fn source_byte_allowance_is_shared_across_manifests_before_decode() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.manifest(1, &["a"])?;
    fixture.manifest(2, &["b"])?;
    let admitted = usize::try_from(fs::metadata(fixture.store.manifest_path("session", 2))?.len())?;
    fs::write(fixture.store.manifest_path("session", 1), b"invalid JSON")?;
    let mut operation = CheckpointOperation::default().read_limits(100, 1024 * 1024, admitted);
    assert!(matches!(
        fixture
            .store
            .build_rewind_steps("session", 0, &mut operation),
        Err(CheckpointError::OperationLimit(_))
    ));
    assert!(!fixture.store.rewind_path("session").exists());
    Ok(())
}

#[test]
fn rewind_steps_reject_retained_growth_and_preserve_source_order() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.manifest(1, &["a"])?;
    fixture.manifest(2, &["a", "z"])?;
    fixture.manifest(3, &["b"])?;
    // Three turn selectors plus only one step fit the count allowance.
    let mut small = CheckpointOperation::default().read_limits(4, 1024 * 1024, 1024 * 1024);
    assert!(matches!(
        fixture.store.build_rewind_steps("session", 0, &mut small),
        Err(CheckpointError::OperationLimit(_))
    ));
    let steps =
        fixture
            .store
            .build_rewind_steps("session", 0, &mut CheckpointOperation::default())?;
    let actual = steps
        .iter()
        .map(|step| match &step.state {
            CheckpointFileState::Unrestorable { reason } => (step.path.as_str(), reason.as_str()),
            _ => ("unexpected", "unexpected"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            ("b", "turn 3"),
            ("a", "turn 2"),
            ("z", "turn 2"),
            ("a", "turn 1")
        ]
    );
    Ok(())
}

#[test]
fn review_union_admits_unique_keys_and_preserves_earliest_baseline() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.manifest(1, &["a"])?;
    fixture.manifest(2, &["a"])?;
    fixture.manifest(3, &["a", "b"])?;
    let mut small = CheckpointOperation::default().read_limits(4, 1024 * 1024, 1024 * 1024);
    assert!(matches!(
        fixture.store.cumulative_baselines("session", &mut small),
        Err(CheckpointError::OperationLimit(_))
    ));
    let baselines = fixture
        .store
        .cumulative_baselines("session", &mut CheckpointOperation::default())?;
    assert_eq!(baselines.len(), 2);
    assert_eq!(
        baselines["a"],
        CheckpointFileState::Unrestorable {
            reason: "turn 1".to_owned()
        }
    );
    assert_eq!(
        baselines["b"],
        CheckpointFileState::Unrestorable {
            reason: "turn 3".to_owned()
        }
    );
    Ok(())
}

#[test]
fn review_file_limit_is_checked_during_union_not_after_a_lifetime_map() -> TestResult {
    let fixture = Fixture::new()?;
    let files = (0..=MAX_REVIEW_FILES)
        .map(|index| (format!("file-{index}"), CheckpointFileState::Absent))
        .collect();
    fixture.store.persist_manifest("session", 1, files)?;
    assert!(matches!(
        fixture
            .store
            .cumulative_baselines("session", &mut CheckpointOperation::default()),
        Err(CheckpointError::ReviewFileLimit)
    ));
    Ok(())
}

#[test]
fn retained_bytes_can_reject_a_few_large_keys_before_union_growth() -> TestResult {
    let fixture = Fixture::new()?;
    let path = "a".repeat(8192);
    fixture.manifest(1, &[&path])?;
    let mut operation = CheckpointOperation::default().read_limits(100, 8192, 1024 * 1024);
    assert!(matches!(
        fixture
            .store
            .cumulative_baselines("session", &mut operation),
        Err(CheckpointError::OperationLimit(_))
    ));
    assert!(fixture.store.load_manifest("session", 1).is_ok());
    Ok(())
}

#[test]
fn review_ledger_rejects_oversized_source_before_reading_it() -> TestResult {
    let fixture = Fixture::new()?;
    File::create(fixture.store.review_path("session"))?
        .set_len(operation::MAX_METADATA_BYTES as u64 + 1)?;
    assert!(matches!(
        fixture.store.session_review("session"),
        Err(CheckpointError::OperationLimit(_))
    ));
    Ok(())
}

#[test]
fn cancellation_interrupts_a_waiting_reference_writer_without_database_creation() -> TestResult {
    let fixture = Fixture::new()?;
    let mut operation = CheckpointOperation::default();
    let held = fixture.store.blobs.lock_references(&mut operation)?;
    let cancellation = operation.cancellation();
    let blobs = fixture.store.blobs.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = started_tx.send(());
        let result = blobs.lock_references(&mut operation).map(drop);
        let _ = done_tx.send(result);
    });
    started_rx.recv()?;
    cancellation.cancel();
    let result = done_rx.recv_timeout(std::time::Duration::from_secs(3));
    drop(held);
    worker.join().map_err(|_| "reference writer panicked")?;
    assert!(matches!(result?, Err(CheckpointError::Cancelled)));
    assert!(
        !fixture
            .store
            .blobs
            .directory()
            .with_file_name("quota.sqlite")
            .exists()
    );
    Ok(())
}
