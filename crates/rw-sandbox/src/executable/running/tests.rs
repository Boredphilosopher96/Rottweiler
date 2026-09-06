#![allow(clippy::expect_used)]

use std::{path::Path, process::Command};

use super::ApprovedExecutable;

const CHILD_RECEIPT: &str = "ROTTWEILER_RUNNING_IMAGE_CLEANUP_TEST_RECEIPT";

#[test]
fn running_image_cache_releases_process_snapshot() {
    if let Some(receipt) = std::env::var_os(CHILD_RECEIPT) {
        capture_and_retire(Path::new(&receipt));
        return;
    }
    let root = tempfile::tempdir().expect("subprocess receipt directory");
    let receipt = root.path().join("snapshots.json");
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "executable::running::tests::running_image_cache_releases_process_snapshot",
            "--nocapture",
        ])
        .env(CHILD_RECEIPT, &receipt)
        .output()
        .expect("run actual helper ownership subprocess");
    assert!(
        output.status.success(),
        "snapshot subprocess failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshots: Vec<std::path::PathBuf> =
        serde_json::from_slice(&std::fs::read(receipt).expect("snapshot paths"))
            .expect("snapshot path receipt");
    assert_eq!(snapshots.len(), 2);
    for snapshot in snapshots {
        assert!(!snapshot.exists(), "subprocess left snapshot bytes on disk");
        assert!(
            !snapshot.parent().expect("snapshot directory").exists(),
            "subprocess left its private snapshot directory on disk"
        );
    }
}

fn capture_and_retire(receipt: &Path) {
    let executable = std::env::current_exe().expect("running subprocess path");
    let first = ApprovedExecutable::from_running(&executable).expect("capture running image");
    let second = ApprovedExecutable::from_running(&executable).expect("reuse live snapshot");
    let first_path = first.launch_path().to_path_buf();
    assert_eq!(first_path, second.launch_path());
    let launch = first.launch().expect("physical launch authority");
    drop(first);
    drop(second);
    assert!(
        first_path.exists(),
        "actual launch owner retains snapshot bytes"
    );
    drop(launch);
    assert!(
        !first_path.exists(),
        "cache must not own retired snapshot bytes"
    );
    let replacement =
        ApprovedExecutable::from_running(&executable).expect("capture next live owner");
    let next_path = replacement.launch_path().to_path_buf();
    assert_ne!(first_path, next_path);
    assert!(next_path.exists());
    std::fs::write(
        receipt,
        serde_json::to_vec(&[first_path, next_path]).expect("snapshot receipt"),
    )
    .expect("publish exact snapshot paths");
    // Normal scope exit retires the final owner before the subprocess exits.
}
