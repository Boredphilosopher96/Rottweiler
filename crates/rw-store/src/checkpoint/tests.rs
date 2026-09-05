use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
};

use rw_types::{ReviewFileDecision, ReviewFileStatus};
use tempfile::tempdir;

use super::{CheckpointFileState, CheckpointStore, RewindReport, render_whole_file_diff};

#[test]
fn oversized_preimage_is_refused_before_a_manifest_is_published()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempdir()?;
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace)?;
    let file = fs::File::create(workspace.join("huge.bin"))?;
    file.set_len(super::MAX_CAPTURE_FILE_BYTES + 1)?;
    let store = CheckpointStore::open(&root.path().join("storage"), &workspace)?;
    assert!(matches!(
        store.checkpoint_known("session", 1, [PathBuf::from("huge.bin")]),
        Err(super::CheckpointError::CaptureFileLimit)
    ));
    assert!(!store.manifest_path("session", 1).exists());
    assert_eq!(fs::read_dir(store.root.join("blobs"))?.count(), 0);
    Ok(())
}

#[test]
fn capture_reads_fixed_chunks_and_cleans_up_partial_failure()
-> Result<(), Box<dyn std::error::Error>> {
    struct Source {
        remaining: usize,
        fail: bool,
    }
    impl std::io::Read for Source {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            assert!(buffer.len() <= super::CAPTURE_CHUNK_BYTES);
            if self.remaining == 0 {
                return if self.fail {
                    Err(std::io::Error::other("injected read failure"))
                } else {
                    Ok(0)
                };
            }
            let count = buffer.len().min(self.remaining);
            buffer[..count].fill(b'x');
            self.remaining -= count;
            Ok(count)
        }
    }
    let root = tempdir()?;
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace)?;
    let store = CheckpointStore::open(&root.path().join("storage"), &workspace)?;
    let mut failed = Source {
        remaining: 2 * super::CAPTURE_CHUNK_BYTES,
        fail: true,
    };
    assert!(store.capture_reader(&mut failed, None).is_err());
    assert_eq!(fs::read_dir(store.root.join("blobs"))?.count(), 0);
    let length = 5 * super::CAPTURE_CHUNK_BYTES + 7;
    let state = store.capture_reader(
        &mut Source {
            remaining: length,
            fail: false,
        },
        None,
    )?;
    let CheckpointFileState::Present { blob, bytes, .. } = state else {
        panic!("capture missing");
    };
    assert_eq!(bytes, length as u64);
    assert_eq!(store.read_valid_blob(&blob, bytes)?, vec![b'x'; length]);
    let duplicate = store.capture_reader(
        &mut Source {
            remaining: length,
            fail: false,
        },
        None,
    )?;
    assert_eq!(
        duplicate,
        CheckpointFileState::Present {
            blob: blob.clone(),
            bytes,
            unix_mode: None
        }
    );
    fs::write(
        store.root.join("blobs").join(&blob[..2]).join(&blob),
        b"corrupt",
    )?;
    assert!(matches!(
        store.capture_reader(
            &mut Source {
                remaining: length,
                fail: false
            },
            None
        ),
        Err(super::CheckpointError::CorruptBlob)
    ));
    Ok(())
}

fn rewind(
    store: &CheckpointStore,
    session_id: &str,
    target_turn: u64,
    operation_id: &str,
) -> RewindReport {
    let handle = store
        .prepare_rewind(session_id, target_turn, operation_id)
        .unwrap_or_else(|error| panic!("rewind must prepare: {error}"));
    let commit = store
        .apply_rewind(&handle)
        .unwrap_or_else(|error| panic!("rewind must apply: {error}"));
    store
        .acknowledge_rewind(&handle)
        .unwrap_or_else(|error| panic!("rewind must acknowledge: {error}"));
    commit.report
}

fn git(workspace: &std::path::Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("git must run: {error}"));
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn ten_edits_rewind_to_turn_three_byte_identically() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    let storage = root.path().join("store");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    let path = workspace.join("counter.txt");
    fs::write(&path, b"turn-0\n")
        .unwrap_or_else(|error| panic!("initial file must write: {error}"));
    let store = CheckpointStore::open(&storage, &workspace)
        .unwrap_or_else(|error| panic!("checkpoint store must open: {error}"));
    for turn in 1_u64..=10 {
        store
            .checkpoint_known("session", turn, [PathBuf::from("counter.txt")])
            .unwrap_or_else(|error| panic!("turn {turn} must checkpoint: {error}"));
        fs::write(&path, format!("turn-{turn}\n"))
            .unwrap_or_else(|error| panic!("turn {turn} must write: {error}"));
    }
    let expected = b"turn-3\n".to_vec();
    let report = rewind(&store, "session", 3, "rewind-3");
    assert_eq!(
        fs::read(path).unwrap_or_else(|error| panic!("rewound file must read: {error}")),
        expected
    );
    assert_eq!(report.restored.len(), 7);
    assert!(report.unrestorable.is_empty());
}

#[test]
fn new_files_are_removed_and_unknown_shell_outputs_are_honest() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    let store = CheckpointStore::open(&root.path().join("store"), &workspace)
        .unwrap_or_else(|error| panic!("checkpoint store must open: {error}"));
    let mut manifest = store
        .checkpoint_known("session", 1, [PathBuf::from("created.txt")])
        .unwrap_or_else(|error| panic!("missing file must checkpoint: {error}"));
    fs::write(workspace.join("created.txt"), b"new")
        .unwrap_or_else(|error| panic!("new file must write: {error}"));
    fs::write(workspace.join("opaque.txt"), b"unknown")
        .unwrap_or_else(|error| panic!("opaque file must write: {error}"));
    store
        .mark_unrestorable(
            &mut manifest,
            [PathBuf::from("opaque.txt")],
            "created by opaque shell execution before its prior state was captured",
        )
        .unwrap_or_else(|error| panic!("unrestorable path must persist: {error}"));
    assert!(matches!(
        manifest.files["created.txt"],
        CheckpointFileState::Absent
    ));
    let report = rewind(&store, "session", 0, "rewind-0");
    assert!(!workspace.join("created.txt").exists());
    assert_eq!(
        fs::read(workspace.join("opaque.txt"))
            .unwrap_or_else(|error| panic!("opaque output must remain: {error}")),
        b"unknown"
    );
    assert!(report.unrestorable.contains_key("opaque.txt"));
}

#[test]
fn repeated_mutations_in_one_turn_preserve_the_earliest_pre_state() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    let file = workspace.join("file.txt");
    fs::write(&file, b"original").unwrap_or_else(|error| panic!("fixture must write: {error}"));
    let store = CheckpointStore::open(&root.path().join("store"), &workspace)
        .unwrap_or_else(|error| panic!("checkpoint store must open: {error}"));

    store
        .checkpoint_known("session", 1, [PathBuf::from("file.txt")])
        .unwrap_or_else(|error| panic!("first mutation must checkpoint: {error}"));
    fs::write(&file, b"intermediate")
        .unwrap_or_else(|error| panic!("intermediate fixture must write: {error}"));
    store
        .checkpoint_known("session", 1, [PathBuf::from("file.txt")])
        .unwrap_or_else(|error| panic!("second mutation must checkpoint: {error}"));
    fs::write(&file, b"final").unwrap_or_else(|error| panic!("final fixture must write: {error}"));

    rewind(&store, "session", 0, "rewind-original");
    assert_eq!(
        fs::read(file).unwrap_or_else(|error| panic!("rewound file must read: {error}")),
        b"original"
    );
}

#[test]
fn traversal_and_symlink_capture_fail_closed() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    let store = CheckpointStore::open(&root.path().join("store"), &workspace)
        .unwrap_or_else(|error| panic!("checkpoint store must open: {error}"));
    assert!(
        store
            .checkpoint_known("session", 1, [PathBuf::from("../escape")])
            .is_err()
    );
    fs::write(workspace.join("safe.txt"), b"safe")
        .unwrap_or_else(|error| panic!("safe fixture must write: {error}"));
    let mut manifest = store
        .checkpoint_known("corrupt", 1, [PathBuf::from("safe.txt")])
        .unwrap_or_else(|error| panic!("safe fixture must checkpoint: {error}"));
    manifest.files.insert(
        "safe.txt".to_owned(),
        CheckpointFileState::Present {
            blob: "../../outside".to_owned(),
            bytes: 4,
            unix_mode: None,
        },
    );
    let bytes = serde_json::to_vec(&manifest)
        .unwrap_or_else(|error| panic!("corrupt fixture must encode: {error}"));
    fs::write(store.manifest_path("corrupt", 1), bytes)
        .unwrap_or_else(|error| panic!("corrupt fixture must write: {error}"));
    assert!(store.load_manifest("corrupt", 1).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("../outside", workspace.join("link"))
            .unwrap_or_else(|error| panic!("fixture symlink must create: {error}"));
        assert!(
            store
                .checkpoint_known("session", 2, [PathBuf::from("link")])
                .is_err()
        );
        fs::create_dir_all(root.path().join("outside"))
            .unwrap_or_else(|error| panic!("outside fixture must create: {error}"));
        std::os::unix::fs::symlink(root.path().join("outside"), workspace.join("parent-link"))
            .unwrap_or_else(|error| panic!("parent symlink must create: {error}"));
        assert!(
            store
                .checkpoint_known("session", 3, [PathBuf::from("parent-link/escape.txt")])
                .is_err()
        );
    }
}

#[cfg(unix)]
#[test]
fn rewind_replaces_final_symlinks_without_touching_their_targets_and_restores_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    let outside = root.path().join("outside.txt");
    fs::write(&outside, b"outside")
        .unwrap_or_else(|error| panic!("outside fixture must write: {error}"));
    let present = workspace.join("present.txt");
    fs::write(&present, b"original")
        .unwrap_or_else(|error| panic!("present fixture must write: {error}"));
    fs::set_permissions(&present, fs::Permissions::from_mode(0o600))
        .unwrap_or_else(|error| panic!("fixture mode must set: {error}"));
    let store = CheckpointStore::open(&root.path().join("store"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    store
        .checkpoint_known(
            "session",
            1,
            [PathBuf::from("present.txt"), PathBuf::from("absent.txt")],
        )
        .unwrap_or_else(|error| panic!("paths must checkpoint: {error}"));
    fs::remove_file(&present)
        .unwrap_or_else(|error| panic!("present fixture must remove: {error}"));
    std::os::unix::fs::symlink(&outside, &present)
        .unwrap_or_else(|error| panic!("replacement symlink must create: {error}"));
    std::os::unix::fs::symlink(&outside, workspace.join("absent.txt"))
        .unwrap_or_else(|error| panic!("new symlink must create: {error}"));

    rewind(&store, "session", 0, "symlink-rewind");
    assert_eq!(
        fs::read(&present).unwrap_or_else(|error| panic!("restored file must read: {error}")),
        b"original"
    );
    assert!(
        !fs::symlink_metadata(&present)
            .unwrap_or_else(|error| panic!("restored metadata must read: {error}"))
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::metadata(&present)
            .unwrap_or_else(|error| panic!("restored mode must read: {error}"))
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    assert!(!workspace.join("absent.txt").exists());
    assert_eq!(
        fs::read(outside).unwrap_or_else(|error| panic!("outside must read: {error}")),
        b"outside"
    );
}

#[test]
fn stale_private_manifest_temp_is_recovered_but_unrecognized_entries_fail_closed() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    let storage = root.path().join("store");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    fs::write(workspace.join("file.txt"), b"before")
        .unwrap_or_else(|error| panic!("fixture must write: {error}"));
    let store = CheckpointStore::open(&storage, &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    store
        .checkpoint_known("session", 1, [PathBuf::from("file.txt")])
        .unwrap_or_else(|error| panic!("fixture must checkpoint: {error}"));
    let manifest_directory = store.root.join("manifests/session");
    fs::write(manifest_directory.join(".rw-123-7.tmp"), b"partial")
        .unwrap_or_else(|error| panic!("stale temp must write: {error}"));
    drop(store);
    let reopened = CheckpointStore::open(&storage, &workspace)
        .unwrap_or_else(|error| panic!("store must recover stale temp: {error}"));
    assert!(!manifest_directory.join(".rw-123-7.tmp").exists());
    fs::write(workspace.join("file.txt"), b"after")
        .unwrap_or_else(|error| panic!("mutation must write: {error}"));
    rewind(&reopened, "session", 0, "temp-rewind");
    assert_eq!(
        fs::read(workspace.join("file.txt"))
            .unwrap_or_else(|error| panic!("restored file must read: {error}")),
        b"before"
    );

    fs::write(manifest_directory.join("unexpected.json.bak"), b"junk")
        .unwrap_or_else(|error| panic!("junk entry must write: {error}"));
    assert!(
        reopened
            .prepare_rewind("session", 0, "junk-rewind")
            .is_err()
    );
}

#[test]
fn rewind_prevalidates_every_blob_before_mutating_workspace() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    fs::write(workspace.join("a.txt"), b"a-before")
        .unwrap_or_else(|error| panic!("a fixture must write: {error}"));
    fs::write(workspace.join("b.txt"), b"b-before")
        .unwrap_or_else(|error| panic!("b fixture must write: {error}"));
    let store = CheckpointStore::open(&root.path().join("store"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    let manifest = store
        .checkpoint_known(
            "session",
            1,
            [PathBuf::from("a.txt"), PathBuf::from("b.txt")],
        )
        .unwrap_or_else(|error| panic!("fixtures must checkpoint: {error}"));
    fs::write(workspace.join("a.txt"), b"a-after")
        .unwrap_or_else(|error| panic!("a mutation must write: {error}"));
    fs::write(workspace.join("b.txt"), b"b-after")
        .unwrap_or_else(|error| panic!("b mutation must write: {error}"));
    let CheckpointFileState::Present { blob, .. } = &manifest.files["b.txt"] else {
        panic!("b must have a blob")
    };
    fs::write(
        store.root.join("blobs").join(&blob[..2]).join(blob),
        b"corrupt",
    )
    .unwrap_or_else(|error| panic!("blob corruption must write: {error}"));
    assert!(
        store
            .prepare_rewind("session", 0, "corrupt-rewind")
            .is_err()
    );
    assert_eq!(
        fs::read(workspace.join("a.txt"))
            .unwrap_or_else(|error| panic!("a current file must read: {error}")),
        b"a-after"
    );
    assert_eq!(
        fs::read(workspace.join("b.txt"))
            .unwrap_or_else(|error| panic!("b current file must read: {error}")),
        b"b-after"
    );
}

#[test]
fn rewind_recovers_idempotently_after_apply_before_progress_persist() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    let storage = root.path().join("store");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    fs::write(workspace.join("file.txt"), b"before")
        .unwrap_or_else(|error| panic!("fixture must write: {error}"));
    let store = CheckpointStore::open(&storage, &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    store
        .checkpoint_known("session", 1, [PathBuf::from("file.txt")])
        .unwrap_or_else(|error| panic!("fixture must checkpoint: {error}"));
    fs::write(workspace.join("file.txt"), b"after")
        .unwrap_or_else(|error| panic!("mutation must write: {error}"));
    let handle = store
        .prepare_rewind("session", 0, "crash-rewind")
        .unwrap_or_else(|error| panic!("rewind must prepare: {error}"));
    let transaction = store
        .load_rewind_transaction("session")
        .unwrap_or_else(|error| panic!("transaction must load: {error}"));
    let mut discarded_report = RewindReport::default();
    store
        .restore_state(
            &transaction.steps[0].path,
            &transaction.steps[0].state,
            &mut discarded_report,
        )
        .unwrap_or_else(|error| panic!("first unrecorded apply must work: {error}"));
    drop(store);

    let reopened = CheckpointStore::open(&storage, &workspace)
        .unwrap_or_else(|error| panic!("store must reopen: {error}"));
    let recovered = reopened
        .recover_rewinds()
        .unwrap_or_else(|error| panic!("rewind must recover: {error}"));
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].handle, handle);
    assert_eq!(
        fs::read(workspace.join("file.txt"))
            .unwrap_or_else(|error| panic!("restored file must read: {error}")),
        b"before"
    );
    assert_eq!(
        reopened
            .recover_rewinds()
            .unwrap_or_else(|error| panic!("committed recovery must repeat: {error}")),
        recovered
    );
    reopened
        .acknowledge_rewind(&handle)
        .unwrap_or_else(|error| panic!("recovered rewind must ack: {error}"));
    assert!(reopened.recover_rewinds().unwrap_or_default().is_empty());
}

#[test]
fn opaque_git_baseline_restores_tracked_marks_unknown_and_removes_new() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("tracked.txt"), b"tracked-before")
        .unwrap_or_else(|error| panic!("tracked fixture must write: {error}"));
    git(&workspace, &["add", "tracked.txt"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=Rottweiler Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    fs::write(workspace.join("unknown.txt"), b"unknown-before")
        .unwrap_or_else(|error| panic!("unknown fixture must write: {error}"));
    let store = CheckpointStore::open(&root.path().join("store"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    let mutation = store
        .begin_opaque_mutation("session", 1)
        .unwrap_or_else(|error| panic!("opaque baseline must begin: {error}"));
    fs::write(workspace.join("tracked.txt"), b"tracked-after")
        .unwrap_or_else(|error| panic!("tracked mutation must write: {error}"));
    fs::write(workspace.join("unknown.txt"), b"unknown-after")
        .unwrap_or_else(|error| panic!("unknown mutation must write: {error}"));
    fs::write(workspace.join("created.txt"), b"created")
        .unwrap_or_else(|error| panic!("created fixture must write: {error}"));
    let manifest = store
        .finish_opaque_mutation(&mutation)
        .unwrap_or_else(|error| panic!("opaque post-scan must finish: {error}"));
    assert!(matches!(
        manifest.files["tracked.txt"],
        CheckpointFileState::Present { .. }
    ));
    assert!(matches!(
        manifest.files["unknown.txt"],
        CheckpointFileState::Unrestorable { .. }
    ));
    assert!(matches!(
        manifest.files["created.txt"],
        CheckpointFileState::Absent
    ));

    let report = rewind(&store, "session", 0, "opaque-rewind");
    assert_eq!(
        fs::read(workspace.join("tracked.txt"))
            .unwrap_or_else(|error| panic!("tracked restore must read: {error}")),
        b"tracked-before"
    );
    assert_eq!(
        fs::read(workspace.join("unknown.txt"))
            .unwrap_or_else(|error| panic!("unknown result must read: {error}")),
        b"unknown-after"
    );
    assert!(!workspace.join("created.txt").exists());
    assert!(report.unrestorable.contains_key("unknown.txt"));
}

#[cfg(unix)]
#[test]
fn failed_git_dirty_query_snapshots_all_tracked_worktree_preimages() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    git(&workspace, &["init", "-q"]);
    let tracked = workspace.join("tracked.txt");
    fs::write(&tracked, b"index-version")
        .unwrap_or_else(|error| panic!("tracked fixture must write: {error}"));
    git(&workspace, &["add", "tracked.txt"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=Rottweiler Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    fs::write(&tracked, b"dirty-worktree-preimage")
        .unwrap_or_else(|error| panic!("dirty preimage must write: {error}"));

    let fake_git = root.path().join("fake-git");
    fs::write(
        &fake_git,
        br#"#!/bin/sh
workspace="$2"
shift 2
if [ "$1" = "diff" ]; then
  exit 73
fi
exec git -C "$workspace" "$@"
"#,
    )
    .unwrap_or_else(|error| panic!("fake git must write: {error}"));
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("fake git must be executable: {error}"));

    let store = CheckpointStore::open(&root.path().join("store"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"))
        .with_git_program(fake_git);
    let mutation = store
        .begin_opaque_mutation("session", 1)
        .unwrap_or_else(|error| panic!("failed diff must use conservative baseline: {error}"));
    fs::write(&tracked, b"agent-after")
        .unwrap_or_else(|error| panic!("agent mutation must write: {error}"));
    let manifest = store
        .finish_opaque_mutation(&mutation)
        .unwrap_or_else(|error| panic!("opaque mutation must finish: {error}"));
    assert!(matches!(
        manifest.files["tracked.txt"],
        CheckpointFileState::Present { .. }
    ));

    rewind(&store, "session", 0, "failed-diff-rewind");
    assert_eq!(
        fs::read(tracked)
            .unwrap_or_else(|error| panic!("restored dirty preimage must read: {error}")),
        b"dirty-worktree-preimage"
    );
}

#[cfg(unix)]
#[test]
fn opaque_recovery_does_not_follow_workspace_symlinks_and_rejects_corrupt_marker() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    let storage = root.path().join("store");
    let outside = root.path().join("outside");
    fs::create_dir_all(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    fs::create_dir_all(&outside).unwrap_or_else(|error| panic!("outside must create: {error}"));
    fs::write(outside.join("secret.txt"), b"secret")
        .unwrap_or_else(|error| panic!("outside secret must write: {error}"));
    std::os::unix::fs::symlink(&outside, workspace.join("link"))
        .unwrap_or_else(|error| panic!("symlink must create: {error}"));
    let store = CheckpointStore::open(&storage, &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    let mutation = store
        .begin_opaque_mutation("session", 1)
        .unwrap_or_else(|error| panic!("opaque baseline must begin: {error}"));
    let pending = store
        .load_pending("session", 1)
        .unwrap_or_else(|error| panic!("pending marker must load: {error}"));
    assert!(pending.before.contains_key("link"));
    assert!(!pending.before.contains_key("link/secret.txt"));
    drop(store);

    let reopened = CheckpointStore::open(&storage, &workspace)
        .unwrap_or_else(|error| panic!("store must reopen: {error}"));
    assert_eq!(
        reopened
            .recover_opaque_mutations()
            .unwrap_or_else(|error| panic!("pending mutation must recover: {error}"))
            .len(),
        1
    );
    assert!(!reopened.pending_path("session", 1).exists());

    let second = reopened
        .begin_opaque_mutation("session", 2)
        .unwrap_or_else(|error| panic!("second baseline must begin: {error}"));
    let path = reopened.pending_path("session", 2);
    let mut value: serde_json::Value = serde_json::from_slice(
        &fs::read(&path).unwrap_or_else(|error| panic!("pending bytes must read: {error}")),
    )
    .unwrap_or_else(|error| panic!("pending JSON must decode: {error}"));
    value["before"]["link"]["target"] = serde_json::Value::String("../../outside".to_owned());
    fs::write(
        &path,
        serde_json::to_vec(&value)
            .unwrap_or_else(|error| panic!("corrupt pending must encode: {error}")),
    )
    .unwrap_or_else(|error| panic!("corrupt pending must write: {error}"));
    assert!(reopened.finish_opaque_mutation(&second).is_err());
    assert!(path.exists());
    assert_eq!(mutation.session_id, "session");
}

#[test]
#[allow(clippy::too_many_lines)]
fn cumulative_review_ten_edits_reverts_one_file_and_preserves_accepted_peer() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    fs::write(workspace.join("alpha.txt"), b"alpha original\n")
        .unwrap_or_else(|error| panic!("alpha baseline must write: {error}"));
    fs::write(workspace.join("beta.txt"), b"beta original\n")
        .unwrap_or_else(|error| panic!("beta baseline must write: {error}"));
    let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));

    for turn in 1..=10_u64 {
        let (path, content) = if turn.is_multiple_of(2) {
            ("beta.txt", format!("beta edit {turn}\n"))
        } else {
            ("alpha.txt", format!("alpha edit {turn}\n"))
        };
        store
            .checkpoint_known("session", turn, [PathBuf::from(path)])
            .unwrap_or_else(|error| panic!("turn {turn} must checkpoint: {error}"));
        fs::write(workspace.join(path), content)
            .unwrap_or_else(|error| panic!("turn {turn} must edit: {error}"));
    }

    let review = store
        .session_review("session")
        .unwrap_or_else(|error| panic!("review must load: {error}"));
    assert_eq!(
        review
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["alpha.txt", "beta.txt"]
    );
    assert!(review.files[0].unified_diff.contains("-alpha original"));
    assert!(review.files[0].unified_diff.contains("+alpha edit 9"));
    assert!(review.files[1].unified_diff.contains("-beta original"));
    assert!(review.files[1].unified_diff.contains("+beta edit 10"));
    let beta_hash = review.files[1].current_hash.clone();

    let accepted = store
        .resolve_review_file(
            "session",
            Path::new("beta.txt"),
            ReviewFileDecision::Accept,
            &beta_hash,
        )
        .unwrap_or_else(|error| panic!("beta must accept: {error}"));
    assert_eq!(
        accepted
            .files
            .iter()
            .find(|file| file.path == "beta.txt")
            .map(|file| file.status),
        Some(ReviewFileStatus::Accepted)
    );
    let alpha_hash = accepted
        .files
        .iter()
        .find(|file| file.path == "alpha.txt")
        .map_or_else(
            || panic!("alpha review entry must remain"),
            |file| file.current_hash.clone(),
        );

    let reverted = store
        .resolve_review_file(
            "session",
            Path::new("alpha.txt"),
            ReviewFileDecision::Revert,
            &alpha_hash,
        )
        .unwrap_or_else(|error| panic!("alpha must revert: {error}"));
    assert_eq!(
        fs::read(workspace.join("alpha.txt"))
            .unwrap_or_else(|error| panic!("alpha result must read: {error}")),
        b"alpha original\n"
    );
    assert_eq!(
        fs::read(workspace.join("beta.txt"))
            .unwrap_or_else(|error| panic!("beta result must read: {error}")),
        b"beta edit 10\n"
    );
    assert_eq!(
        reverted
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.status))
            .collect::<Vec<_>>(),
        [
            ("alpha.txt", ReviewFileStatus::Reverted),
            ("beta.txt", ReviewFileStatus::Accepted),
        ]
    );

    fs::write(
        workspace.join("beta.txt"),
        b"beta changed after acceptance\n",
    )
    .unwrap_or_else(|error| panic!("post-accept edit must write: {error}"));
    assert!(matches!(
        store.resolve_review_file(
            "session",
            Path::new("beta.txt"),
            ReviewFileDecision::Accept,
            &beta_hash,
        ),
        Err(super::CheckpointError::ReviewPathChanged)
    ));
    let changed = store
        .session_review("session")
        .unwrap_or_else(|error| panic!("changed review must load: {error}"));
    assert_eq!(
        changed
            .files
            .iter()
            .find(|file| file.path == "beta.txt")
            .map(|file| file.status),
        Some(ReviewFileStatus::Pending)
    );
}

#[test]
fn review_diff_has_minimal_context_and_handles_file_edge_cases() {
    let original = b"one\ntwo\nthree\nfour\nfive\n";
    let current = b"one\ntwo\nTHREE\nfour\nfive\n";
    let (edited, truncated) =
        render_whole_file_diff("file.txt", Some(original), Some(current), 16 * 1024);
    assert!(!truncated);
    assert!(edited.contains(" two\n-three\n+THREE\n four\n"));
    assert!(!edited.contains("-one\n"));
    assert!(!edited.contains("+one\n"));

    let (deleted, truncated) = render_whole_file_diff("file.txt", Some(b"gone\n"), None, 16 * 1024);
    assert!(!truncated);
    assert!(deleted.contains("+++ /dev/null"));
    assert!(deleted.contains("-gone"));

    let (created, truncated) = render_whole_file_diff("new.txt", None, Some(b"new\n"), 16 * 1024);
    assert!(!truncated);
    assert!(created.contains("--- /dev/null"));
    assert!(created.contains("+new"));

    let (no_newline, truncated) =
        render_whole_file_diff("plain.txt", Some(b"before"), Some(b"after"), 16 * 1024);
    assert!(!truncated);
    assert!(no_newline.contains("\\ No newline at end of file"));

    let (binary, truncated) =
        render_whole_file_diff("binary.dat", Some(&[0xff, 0]), Some(&[0xfe, 0]), 16 * 1024);
    assert!(truncated);
    assert_eq!(binary, "Binary files differ\n");
}

#[cfg(unix)]
#[test]
fn unsupported_symlink_target_swaps_cannot_be_accepted_or_reverted() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    fs::write(workspace.join("review.txt"), b"baseline\n")
        .unwrap_or_else(|error| panic!("baseline must write: {error}"));
    fs::write(workspace.join("first.txt"), b"first\n")
        .unwrap_or_else(|error| panic!("first target must write: {error}"));
    fs::write(workspace.join("second.txt"), b"second\n")
        .unwrap_or_else(|error| panic!("second target must write: {error}"));
    let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    store
        .checkpoint_known("symlink-session", 1, [PathBuf::from("review.txt")])
        .unwrap_or_else(|error| panic!("baseline must checkpoint: {error}"));
    fs::remove_file(workspace.join("review.txt"))
        .unwrap_or_else(|error| panic!("baseline must remove: {error}"));
    symlink("first.txt", workspace.join("review.txt"))
        .unwrap_or_else(|error| panic!("first symlink must create: {error}"));
    let first = store
        .session_review("symlink-session")
        .unwrap_or_else(|error| panic!("first review must load: {error}"));
    assert!(first.files[0].unrestorable_reason.is_some());
    let first_hash = first.files[0].current_hash.clone();
    assert!(matches!(
        store.resolve_review_file(
            "symlink-session",
            Path::new("review.txt"),
            ReviewFileDecision::Accept,
            &first_hash,
        ),
        Err(super::CheckpointError::ReviewPathNotRevertible)
    ));
    fs::remove_file(workspace.join("review.txt"))
        .unwrap_or_else(|error| panic!("first symlink must remove: {error}"));
    symlink("second.txt", workspace.join("review.txt"))
        .unwrap_or_else(|error| panic!("second symlink must create: {error}"));
    let second = store
        .session_review("symlink-session")
        .unwrap_or_else(|error| panic!("second review must load: {error}"));
    assert_eq!(second.files[0].status, ReviewFileStatus::Pending);
    assert!(matches!(
        store.resolve_review_file(
            "symlink-session",
            Path::new("review.txt"),
            ReviewFileDecision::Revert,
            &second.files[0].current_hash,
        ),
        Err(super::CheckpointError::ReviewPathNotRevertible)
    ));
}

#[test]
fn oversized_review_streams_identity_and_remains_safely_revertible() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    let path = workspace.join("large.bin");
    fs::write(&path, b"small baseline\n")
        .unwrap_or_else(|error| panic!("baseline must write: {error}"));
    let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    store
        .checkpoint_known("large-session", 1, [PathBuf::from("large.bin")])
        .unwrap_or_else(|error| panic!("baseline must checkpoint: {error}"));
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("large fixture must open: {error}"));
    file.set_len(8 * 1024 * 1024)
        .unwrap_or_else(|error| panic!("large fixture must resize: {error}"));

    let review = store
        .session_review("large-session")
        .unwrap_or_else(|error| panic!("large review must stream: {error}"));
    assert_eq!(review.files.len(), 1);
    assert!(review.files[0].truncated);
    assert!(review.files[0].unrestorable_reason.is_none());
    let current_hash = review.files[0].current_hash.clone();
    store
        .resolve_review_file(
            "large-session",
            Path::new("large.bin"),
            ReviewFileDecision::Revert,
            &current_hash,
        )
        .unwrap_or_else(|error| panic!("truncated review must revert: {error}"));
    assert_eq!(
        fs::read(path).unwrap_or_else(|error| panic!("reverted file must read: {error}")),
        b"small baseline\n"
    );
}

#[test]
fn huge_sparse_review_is_bounded_and_marked_unreviewable() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    let path = workspace.join("huge.bin");
    fs::write(&path, b"small baseline\n")
        .unwrap_or_else(|error| panic!("baseline must write: {error}"));
    let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    store
        .checkpoint_known("huge-session", 1, [PathBuf::from("huge.bin")])
        .unwrap_or_else(|error| panic!("baseline must checkpoint: {error}"));
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("sparse fixture must open: {error}"))
        .set_len(128 * 1024 * 1024)
        .unwrap_or_else(|error| panic!("sparse fixture must resize: {error}"));
    let started = std::time::Instant::now();
    let review = store
        .session_review("huge-session")
        .unwrap_or_else(|error| panic!("bounded review must load: {error}"));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(review.files.len(), 1);
    assert!(review.files[0].unrestorable_reason.is_some());
}

#[test]
fn checkpoint_fork_rebinds_child_manifests_without_changing_parent() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap_or_else(|error| panic!("workspace must create: {error}"));
    fs::write(workspace.join("file.txt"), b"zero\n")
        .unwrap_or_else(|error| panic!("baseline must write: {error}"));
    let store = CheckpointStore::open(&root.path().join("storage"), &workspace)
        .unwrap_or_else(|error| panic!("store must open: {error}"));
    for turn in 1..=3_u64 {
        store
            .checkpoint_known("parent", turn, [PathBuf::from("file.txt")])
            .unwrap_or_else(|error| panic!("parent checkpoint must write: {error}"));
        fs::write(workspace.join("file.txt"), format!("{turn}\n"))
            .unwrap_or_else(|error| panic!("parent edit must write: {error}"));
    }
    let parent_before = fs::read(store.manifest_path("parent", 1))
        .unwrap_or_else(|error| panic!("parent manifest must read: {error}"));

    let child_store = CheckpointStore::open(&root.path().join("child-storage"), &workspace)
        .unwrap_or_else(|error| panic!("child store must open: {error}"));
    store
        .fork_into(&child_store, "parent", "child", Some(2))
        .unwrap_or_else(|error| panic!("checkpoint fork must succeed: {error}"));
    assert_eq!(
        fs::read(store.manifest_path("parent", 1))
            .unwrap_or_else(|error| panic!("parent manifest must reread: {error}")),
        parent_before
    );
    assert_eq!(
        child_store
            .load_manifest("child", 1)
            .unwrap_or_else(|error| panic!("child manifest one must load: {error}"))
            .session_id,
        "child"
    );
    assert!(child_store.load_manifest("child", 2).is_ok());
    assert!(child_store.load_manifest("child", 3).is_err());
    assert!(matches!(
        store.fork_into(&child_store, "parent", "child", None),
        Err(super::CheckpointError::ForkTargetExists)
    ));
}
