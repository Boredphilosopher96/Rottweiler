#![allow(clippy::expect_used)]

use super::git::run_git_raw_with_paths;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rw_types::{Cost, DiffArtifact, SessionId, Usage};
use serde_json::json;

use crate::bash::audited_system_git;
use crate::registry::{CancellationToken, Tool, ToolContext, ToolError};

use std::process::Command as StdCommand;

use super::*;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = StdCommand::new(audited_system_git().expect("audited git"))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Rottweiler Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Rottweiler Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn repository() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repository tempdir");
    git(repo.path(), &["init", "--quiet"]);
    std::fs::write(repo.path().join("shared.txt"), b"base\n").expect("write base");
    git(repo.path(), &["add", "shared.txt"]);
    git(repo.path(), &["commit", "--quiet", "-m", "base"]);
    repo
}

fn accounting() -> (Usage, Cost) {
    (
        Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            reasoning_tokens: 5,
        },
        Cost::Monetary {
            amount_micros: 7,
            currency: "USD".to_owned(),
        },
    )
}

fn authorized_apply_tool(artifacts: &[&DiffArtifact]) -> (ApplyWorktreeDiffTool, SessionId) {
    let session = SessionId("parent-session".to_owned());
    let authority = Arc::new(SessionDiffArtifactAuthority::default());
    for artifact in artifacts {
        authority
            .record_durable(session.clone(), artifact)
            .expect("record durable artifact");
    }
    (ApplyWorktreeDiffTool::new(authority), session)
}

async fn isolation(repo: &Path, private: &Path) -> WorktreeIsolation {
    WorktreeIsolation::new(
        repo,
        private,
        WorktreeLimits::default(),
        CancellationToken::default(),
    )
    .await
    .expect("create isolation")
}

#[tokio::test]
async fn three_parallel_explorers_leave_parent_diff_untouched() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let (one, two, three) = tokio::join!(
        manager.create(CancellationToken::default()),
        manager.create(CancellationToken::default()),
        manager.create(CancellationToken::default()),
    );
    let leases = [one.expect("one"), two.expect("two"), three.expect("three")];
    for (index, lease) in leases.iter().enumerate() {
        std::fs::write(
            lease.path().join(format!("explorer-{index}.txt")),
            format!("result {index}\n"),
        )
        .expect("write explorer result");
    }
    let parent_before = git(repo.path(), &["diff", "--binary", "HEAD", "--"]);
    assert!(parent_before.is_empty());
    let (usage, cost) = accounting();
    for lease in &leases {
        let artifact = manager
            .collect(
                lease,
                "done",
                usage.clone(),
                cost.clone(),
                CancellationToken::default(),
            )
            .await
            .expect("collect");
        assert!(artifact.diff.is_some());
    }
    assert!(git(repo.path(), &["diff", "--binary", "HEAD", "--"]).is_empty());
    assert!(git(repo.path(), &["status", "--porcelain"]).is_empty());
}

#[tokio::test]
async fn separate_managers_serialize_overlapping_add_and_remove_registry_mutations() {
    let repo = repository();
    let private_one = tempfile::tempdir().expect("first private tempdir");
    let private_two = tempfile::tempdir().expect("second private tempdir");
    let first = isolation(repo.path(), private_one.path()).await;
    let second = isolation(repo.path(), private_two.path()).await;
    assert!(first.registry_state.get().is_none());
    assert!(second.registry_state.get().is_none());

    let existing = first
        .create(CancellationToken::default())
        .await
        .expect("existing lease");
    assert!(first.registry_state.get().is_some());
    assert!(second.registry_state.get().is_none());
    drop(
        second
            .lock_registry(&CancellationToken::default())
            .await
            .expect("initialize second manager gate"),
    );
    assert!(second.registry_state.get().is_some());
    let held = first
        .lock_registry(&CancellationToken::default())
        .await
        .expect("hold registry gate");
    let create_manager = second.clone();
    let create =
        tokio::spawn(async move { create_manager.create(CancellationToken::default()).await });
    let remove_manager = first.clone();
    let remove = tokio::spawn(async move {
        remove_manager
            .cleanup_if_untouched(&existing, CancellationToken::default())
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(
        !create.is_finished(),
        "add bypassed the shared registry gate"
    );
    assert!(
        !remove.is_finished(),
        "remove bypassed the shared registry gate"
    );
    drop(held);

    let created = create
        .await
        .expect("add task")
        .expect("create after gate release");
    assert!(
        remove
            .await
            .expect("remove task")
            .expect("remove after gate release")
    );
    assert!(
        second
            .cleanup_if_untouched(&created, CancellationToken::default())
            .await
            .expect("cleanup created lease")
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn cross_process_registry_lock_refuses_a_symlink() {
    use std::os::unix::fs::symlink;

    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let target = private.path().join("lock-target");
    std::fs::write(&target, "must remain unchanged").expect("lock target");
    symlink(&target, repo.path().join(".git/.rottweiler-worktree.lock"))
        .expect("malicious lock symlink");
    let manager = isolation(repo.path(), private.path()).await;
    let error = manager
        .create(CancellationToken::default())
        .await
        .expect_err("symlink lock must fail closed");
    assert!(error.to_string().contains("Git worktree lock"));
    assert_eq!(
        std::fs::read_to_string(target).expect("unchanged target"),
        "must remain unchanged"
    );
}

#[tokio::test]
async fn isolated_diff_applies_only_through_explicit_tool() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("shared.txt"), b"implemented\n").expect("edit");
    std::fs::write(lease.path().join("--help;touch owned"), b"exact argv\n")
        .expect("write injection-shaped path");
    let (usage, cost) = accounting();
    let child = manager
        .collect(
            &lease,
            "implemented",
            usage,
            cost,
            CancellationToken::default(),
        )
        .await
        .expect("artifact");
    assert_eq!(
        std::fs::read(repo.path().join("shared.txt")).expect("read"),
        b"base\n"
    );
    let artifact = child.diff.expect("diff");
    let (tool, session) = authorized_apply_tool(&[&artifact]);
    let context = ToolContext::new(repo.path())
        .expect("context")
        .with_session_id(session);
    tool.execute(&context, json!({"artifact_id": artifact.id}))
        .await
        .expect("apply");
    assert_eq!(
        std::fs::read(repo.path().join("shared.txt")).expect("read"),
        b"implemented\n"
    );
    assert_eq!(
        std::fs::read(repo.path().join("--help;touch owned")).expect("read exact path"),
        b"exact argv\n"
    );
    assert!(!repo.path().join("owned").exists());
}

#[tokio::test]
async fn conflicting_pair_is_a_tool_error_and_keeps_first_result() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let first = manager
        .create(CancellationToken::default())
        .await
        .expect("first");
    let second = manager
        .create(CancellationToken::default())
        .await
        .expect("second");
    std::fs::write(first.path().join("shared.txt"), b"first\n").expect("first write");
    std::fs::write(second.path().join("shared.txt"), b"second\n").expect("second write");
    let (usage, cost) = accounting();
    let first = manager
        .collect(
            &first,
            "first",
            usage.clone(),
            cost.clone(),
            CancellationToken::default(),
        )
        .await
        .expect("first artifact");
    let second = manager
        .collect(&second, "second", usage, cost, CancellationToken::default())
        .await
        .expect("second artifact");
    let first_artifact = first.diff.expect("first diff");
    let second_artifact = second.diff.expect("second diff");
    let (tool, session) = authorized_apply_tool(&[&first_artifact, &second_artifact]);
    let context = ToolContext::new(repo.path())
        .expect("context")
        .with_session_id(session);
    tool.execute(&context, json!({"artifact": first_artifact}))
        .await
        .expect("first apply");
    let bytes_before = std::fs::read(repo.path().join("shared.txt")).expect("bytes before");
    let status_before = git(repo.path(), &["status", "--porcelain=v1"]);
    let index = git(repo.path(), &["rev-parse", "--git-path", "index"]);
    let index = if Path::new(&index).is_absolute() {
        PathBuf::from(index)
    } else {
        repo.path().join(index)
    };
    let index_before = std::fs::read(&index).expect("index before");
    let error = tool
        .execute(&context, json!({"artifact": second_artifact}))
        .await
        .expect_err("second conflicts");
    assert!(error.to_string().contains("conflict"), "{error}");
    assert_eq!(
        std::fs::read(repo.path().join("shared.txt")).expect("read"),
        bytes_before
    );
    assert_eq!(
        git(repo.path(), &["status", "--porcelain=v1"]),
        status_before
    );
    assert_eq!(std::fs::read(index).expect("index after"), index_before);
    assert!(
        !repo
            .path()
            .join("shared.txt")
            .with_extension("txt.rej")
            .exists()
    );
}

#[tokio::test]
async fn post_preflight_apply_failure_preserves_parent_bytes_and_index() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("shared.txt"), b"child\n").expect("child write");
    let (usage, cost) = accounting();
    let artifact = manager
        .collect(&lease, "done", usage, cost, CancellationToken::default())
        .await
        .expect("collect")
        .diff
        .expect("artifact");

    std::fs::write(repo.path().join("shared.txt"), b"unstaged parent\n").expect("parent write");
    let bytes_before = std::fs::read(repo.path().join("shared.txt")).expect("bytes before");
    let status_before = git(repo.path(), &["status", "--porcelain=v1"]);
    let index = repo
        .path()
        .join(git(repo.path(), &["rev-parse", "--git-path", "index"]));
    let index_before = std::fs::read(&index).expect("index before");
    let (tool, session) = authorized_apply_tool(&[&artifact]);
    let context = ToolContext::new(repo.path())
        .expect("context")
        .with_session_id(session);
    let error = tool
        .execute(&context, json!({"artifact": artifact}))
        .await
        .expect_err("unstaged parent blocks apply");
    assert!(
        error
            .to_string()
            .contains("failed after checkpointed preflight")
    );
    assert_eq!(
        std::fs::read(repo.path().join("shared.txt")).expect("bytes after"),
        bytes_before
    );
    assert_eq!(
        git(repo.path(), &["status", "--porcelain=v1"]),
        status_before
    );
    assert_eq!(std::fs::read(index).expect("index after"), index_before);
}

#[tokio::test]
async fn correctly_hashed_forgery_is_rejected_by_session_authority() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("shared.txt"), b"authorized\n").expect("authorized edit");
    let (usage, cost) = accounting();
    let authorized = manager
        .collect(&lease, "done", usage, cost, CancellationToken::default())
        .await
        .expect("collect")
        .diff
        .expect("artifact");
    let mut forged = authorized.clone();
    forged.unified_diff = forged.unified_diff.replace("authorized", "forged");
    forged.id = artifact_id(
        &forged.base_commit,
        &forged.touched_files,
        &forged.unified_diff,
    )
    .expect("forge valid digest");
    verify_artifact(&forged).expect("unkeyed integrity is internally valid");

    let (tool, session) = authorized_apply_tool(&[&authorized]);
    let context = ToolContext::new(repo.path())
        .expect("context")
        .with_session_id(session);
    let error = tool
        .execute(&context, json!({"artifact": forged}))
        .await
        .expect_err("forged artifact rejected");
    assert!(error.to_string().contains("not durably produced"));
    assert_eq!(
        std::fs::read(repo.path().join("shared.txt")).expect("parent bytes"),
        b"base\n"
    );

    let preview = tool
        .approval_preview(&context, &json!({"artifact": authorized.clone()}))
        .await
        .expect("authorized preview")
        .expect("preview");
    let preview_artifact: DiffArtifact =
        serde_json::from_slice(&preview.after).expect("full artifact preview");
    assert_eq!(preview_artifact, authorized);
}

#[tokio::test]
async fn artifact_reference_is_session_scoped_and_preview_expands_full_artifact() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("shared.txt"), b"referenced\n").expect("referenced edit");
    let (usage, cost) = accounting();
    let artifact = manager
        .collect(&lease, "done", usage, cost, CancellationToken::default())
        .await
        .expect("collect")
        .diff
        .expect("artifact");
    let artifact_id = artifact.id.clone();
    let (tool, session) = authorized_apply_tool(&[&artifact]);
    let context = ToolContext::new(repo.path())
        .expect("context")
        .with_session_id(session);

    let preview = tool
        .approval_preview(&context, &json!({"artifact_id": artifact_id}))
        .await
        .expect("preview")
        .expect("approval preview");
    assert_eq!(
        serde_json::from_slice::<DiffArtifact>(&preview.after).expect("preview artifact"),
        artifact
    );

    let other_context = ToolContext::new(repo.path())
        .expect("other context")
        .with_session_id(SessionId("other-parent".to_owned()));
    let error = tool
        .execute(&other_context, json!({"artifact_id": artifact.id}))
        .await
        .expect_err("cross-session reference rejected");
    assert!(error.to_string().contains("not durably produced"));
}

#[tokio::test]
async fn apply_input_requires_exactly_one_artifact_form() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("shared.txt"), b"exactly-one\n").expect("edit");
    let (usage, cost) = accounting();
    let artifact = manager
        .collect(&lease, "done", usage, cost, CancellationToken::default())
        .await
        .expect("collect")
        .diff
        .expect("artifact");
    let (tool, session) = authorized_apply_tool(&[&artifact]);
    let context = ToolContext::new(repo.path())
        .expect("context")
        .with_session_id(session);

    for input in [
        json!({}),
        json!({"artifact": artifact.clone(), "artifact_id": artifact.id.clone()}),
    ] {
        let error = tool
            .approval_preview(&context, &input)
            .await
            .expect_err("invalid union rejected");
        assert!(error.to_string().contains("exactly one"));
    }
    let malformed = tool
        .approval_preview(&context, &json!({"artifact_id": "not-a-digest"}))
        .await
        .expect_err("malformed reference rejected");
    assert!(malformed.to_string().contains("reference is malformed"));
}

#[tokio::test]
async fn cancellation_and_safe_cleanup_fail_closed() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let error = manager.create(cancellation).await.expect_err("cancelled");
    assert!(matches!(error, ToolError::Cancelled));

    let clean = manager
        .create(CancellationToken::default())
        .await
        .expect("clean");
    let clean_path = clean.path().to_path_buf();
    assert!(
        manager
            .cleanup_if_untouched(&clean, CancellationToken::default())
            .await
            .expect("cleanup")
    );
    assert!(!clean_path.exists());

    let dirty = manager
        .create(CancellationToken::default())
        .await
        .expect("dirty");
    std::fs::write(dirty.path().join("keep.txt"), b"keep\n").expect("dirty write");
    assert!(
        !manager
            .cleanup_if_untouched(&dirty, CancellationToken::default())
            .await
            .expect("refuse dirty cleanup")
    );
    assert!(dirty.path().exists());
}

#[tokio::test]
async fn tombstoned_changed_lease_is_discarded_without_parent_mutation() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("rewound.txt"), b"discard me\n").expect("dirty write");
    let record = lease.durable_record();
    let lease_path = lease.path().to_path_buf();

    manager
        .discard_tombstoned(&record, CancellationToken::default())
        .await
        .expect("discard tombstoned lease");

    assert!(!lease_path.exists());
    manager
        .discard_tombstoned(&record, CancellationToken::default())
        .await
        .expect("already-absent unregistered lease is idempotent");
    assert!(!repo.path().join("rewound.txt").exists());
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
}

#[tokio::test]
async fn tombstoned_discard_honors_cancellation_without_removing_the_lease() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("keep.txt"), b"keep\n").expect("dirty write");
    let record = lease.durable_record();
    let cancellation = CancellationToken::default();
    cancellation.cancel();

    let error = manager
        .discard_tombstoned(&record, cancellation)
        .await
        .expect_err("cancelled discard");

    assert!(matches!(error, ToolError::Cancelled));
    assert!(lease.path().exists());
    assert!(!repo.path().join("keep.txt").exists());
}

#[tokio::test]
async fn tombstoned_discard_rejects_tampered_path_and_base() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    let record = lease.durable_record();

    let mut path_tampered = record.clone();
    path_tampered.path = repo.path().to_path_buf();
    path_tampered.canonical_path = repo.path().to_path_buf();
    assert!(
        manager
            .discard_tombstoned(&path_tampered, CancellationToken::default())
            .await
            .is_err()
    );
    assert!(repo.path().join("shared.txt").exists());

    let mut base_tampered = record.clone();
    base_tampered.base_commit = "0".repeat(40);
    assert!(
        manager
            .discard_tombstoned(&base_tampered, CancellationToken::default())
            .await
            .is_err()
    );
    assert!(lease.path().exists());
    manager
        .discard_tombstoned(&record, CancellationToken::default())
        .await
        .expect("discard untampered lease");
}

#[tokio::test]
async fn tombstoned_discard_rejects_absent_but_registered_worktree() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    let record = lease.durable_record();
    let path = lease.path().to_path_buf();
    let parked = manager.private_root().join("parked-registered-lease");
    std::fs::rename(&path, &parked).expect("park registered lease");

    let error = manager
        .discard_tombstoned(&record, CancellationToken::default())
        .await
        .expect_err("registered missing lease rejected");
    assert!(error.to_string().contains("remains registered"), "{error}");

    std::fs::rename(&parked, &path).expect("restore registered lease");
    manager
        .discard_tombstoned(&record, CancellationToken::default())
        .await
        .expect("discard restored lease");
}

#[cfg(unix)]
#[tokio::test]
async fn tombstoned_discard_rejects_symlink_and_inode_swaps() {
    use std::os::unix::fs::symlink;

    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;

    let symlink_lease = manager
        .create(CancellationToken::default())
        .await
        .expect("symlink lease");
    let symlink_record = symlink_lease.durable_record();
    let symlink_path = symlink_lease.path().to_path_buf();
    let symlink_parked = manager.private_root().join("parked-symlink-lease");
    std::fs::rename(&symlink_path, &symlink_parked).expect("park symlink lease");
    symlink(repo.path(), &symlink_path).expect("replace lease with symlink");
    assert!(
        manager
            .discard_tombstoned(&symlink_record, CancellationToken::default())
            .await
            .is_err()
    );
    assert!(repo.path().join("shared.txt").exists());
    std::fs::remove_file(&symlink_path).expect("remove swap symlink");
    std::fs::rename(&symlink_parked, &symlink_path).expect("restore symlink lease");
    manager
        .discard_tombstoned(&symlink_record, CancellationToken::default())
        .await
        .expect("discard restored symlink lease");

    let inode_lease = manager
        .create(CancellationToken::default())
        .await
        .expect("inode lease");
    let inode_record = inode_lease.durable_record();
    let inode_path = inode_lease.path().to_path_buf();
    let inode_parked = manager.private_root().join("parked-inode-lease");
    std::fs::rename(&inode_path, &inode_parked).expect("park inode lease");
    std::fs::create_dir(&inode_path).expect("replace lease directory");
    assert!(
        manager
            .discard_tombstoned(&inode_record, CancellationToken::default())
            .await
            .is_err()
    );
    assert!(inode_path.exists());
    assert!(inode_parked.exists());
    std::fs::remove_dir(&inode_path).expect("remove replacement directory");
    std::fs::rename(&inode_parked, &inode_path).expect("restore inode lease");
    manager
        .discard_tombstoned(&inode_record, CancellationToken::default())
        .await
        .expect("discard restored inode lease");
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
}

#[tokio::test]
async fn active_git_process_is_reaped_on_cancellation() {
    #[cfg(unix)]
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let repo = repository();
    let input = vec![b'x'; 32 * 1024 * 1024];
    let cancellation = CancellationToken::default();
    let trigger = cancellation.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        trigger.cancel();
    });
    let result = run_git_raw_with_paths(
        repo.path(),
        [OsString::from("hash-object"), OsString::from("--stdin")],
        Some(&input),
        &cancellation,
        None,
        None,
    )
    .await;
    cancel_task.await.expect("cancellation task");
    assert!(matches!(result, Err(ToolError::Cancelled)));
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
}

#[tokio::test]
async fn captured_changes_finalize_only_while_artifact_still_matches() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("new.txt"), b"captured\n").expect("write");
    let (usage, cost) = accounting();
    let child = manager
        .collect(&lease, "done", usage, cost, CancellationToken::default())
        .await
        .expect("collect");
    let artifact = child.diff.expect("diff");
    std::fs::write(lease.path().join("new.txt"), b"changed later\n").expect("change later");
    assert!(
        !manager
            .finalize_captured(&lease, &artifact, CancellationToken::default())
            .await
            .expect("refuse changed finalization")
    );
    assert!(lease.path().exists());
    std::fs::write(lease.path().join("new.txt"), b"captured\n").expect("restore captured");
    assert!(
        manager
            .finalize_captured(&lease, &artifact, CancellationToken::default())
            .await
            .expect("finalize captured")
    );
    assert!(!lease.path().exists());
    assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
}

#[tokio::test]
async fn finalization_rechecks_after_capture_before_forced_removal() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    let target = lease.path().join("new.txt");
    std::fs::write(&target, b"captured\n").expect("captured write");
    let (usage, cost) = accounting();
    let artifact = manager
        .collect(&lease, "done", usage, cost, CancellationToken::default())
        .await
        .expect("collect")
        .diff
        .expect("diff");
    install_finalize_after_capture_test_write(
        lease.path(),
        target.clone(),
        b"late writer\n".to_vec(),
    );

    let error = manager
        .finalize_captured(&lease, &artifact, CancellationToken::default())
        .await
        .expect_err("late write must abort finalization");
    assert!(matches!(
        error,
        ToolError::WorktreeChangedAfterCapture(ref changed) if changed == lease.path()
    ));
    assert!(lease.path().exists());
    assert_eq!(
        std::fs::read_to_string(&target).expect("late output retained"),
        "late writer\n"
    );
}

#[tokio::test]
async fn durable_rebind_continues_in_the_same_worktree() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    std::fs::write(lease.path().join("first.txt"), b"first turn\n").expect("first turn");
    let (usage, cost) = accounting();
    let first = manager
        .collect(
            &lease,
            "first",
            usage.clone(),
            cost.clone(),
            CancellationToken::default(),
        )
        .await
        .expect("first collect");
    assert_eq!(first.touched_files.len(), 1);

    let encoded = serde_json::to_vec(&lease.durable_record()).expect("encode lease");
    let record: WorktreeLeaseRecord = serde_json::from_slice(&encoded).expect("decode lease");
    let rebound = manager
        .rebind(&record, CancellationToken::default())
        .await
        .expect("rebind");
    assert_eq!(rebound.path(), lease.path());
    std::fs::write(rebound.path().join("second.txt"), b"follow-up turn\n").expect("follow-up turn");
    let second = manager
        .collect(
            &rebound,
            "second",
            usage,
            cost,
            CancellationToken::default(),
        )
        .await
        .expect("second collect");
    assert_eq!(
        second
            .touched_files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["first.txt", "second.txt"]
    );
    let artifact = second.diff.expect("cumulative diff");
    assert!(
        manager
            .finalize_captured(&rebound, &artifact, CancellationToken::default())
            .await
            .expect("finalize rebound lease")
    );
}

#[tokio::test]
async fn rejects_nested_roots_subdirectories_and_symlink_swaps() {
    let repo = repository();
    let nested = repo.path().join("nested");
    std::fs::create_dir(&nested).expect("nested");
    let private = tempfile::tempdir().expect("private tempdir");
    let error = WorktreeIsolation::new(
        &nested,
        private.path(),
        WorktreeLimits::default(),
        CancellationToken::default(),
    )
    .await
    .expect_err("subdirectory rejected");
    assert!(error.to_string().contains("exact git top level"));

    let error = WorktreeIsolation::new(
        repo.path(),
        repo.path().join("private"),
        WorktreeLimits::default(),
        CancellationToken::default(),
    )
    .await
    .expect_err("nested private rejected");
    assert!(error.to_string().contains("outside"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let insecure_parent = tempfile::tempdir().expect("insecure parent");
        let insecure = insecure_parent.path().join("shared-private-root");
        std::fs::create_dir(&insecure).expect("insecure root");
        std::fs::set_permissions(&insecure, std::fs::Permissions::from_mode(0o755))
            .expect("insecure mode");
        let private_manager = WorktreeIsolation::new(
            repo.path(),
            &insecure,
            WorktreeLimits::default(),
            CancellationToken::default(),
        )
        .await
        .expect("dedicated private child");
        assert_eq!(
            std::fs::metadata(&insecure)
                .expect("insecure metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(private_manager.private_root())
                .expect("private child metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let aliases = tempfile::tempdir().expect("alias tempdir");
        let alias = aliases.path().join("repo-alias");
        std::os::unix::fs::symlink(repo.path(), &alias).expect("private root alias");
        let escaped = alias.join("must-not-be-created");
        let error = WorktreeIsolation::new(
            repo.path(),
            &escaped,
            WorktreeLimits::default(),
            CancellationToken::default(),
        )
        .await
        .expect_err("symlinked private parent rejected before creation");
        assert!(error.to_string().contains("outside"));
        assert!(!escaped.exists());

        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        let original = lease.path().to_path_buf();
        let moved = private.path().join("moved-worktree");
        std::fs::rename(&original, &moved).expect("move worktree");
        std::os::unix::fs::symlink(repo.path(), &original).expect("swap symlink");
        let (usage, cost) = accounting();
        let error = manager
            .collect(&lease, "unsafe", usage, cost, CancellationToken::default())
            .await
            .expect_err("symlink swap rejected");
        assert!(
            error.to_string().contains("real directory") || error.to_string().contains("identity")
        );
        assert!(git(repo.path(), &["status", "--porcelain"]).is_empty());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn project_hooks_and_filters_never_execute_during_isolation() {
    use std::os::unix::fs::PermissionsExt as _;

    let repo = repository();
    std::fs::write(
        repo.path().join(".gitattributes"),
        "filtered.txt filter=evil\n",
    )
    .expect("attributes");
    std::fs::write(repo.path().join("filtered.txt"), "safe\n").expect("filtered file");
    git(repo.path(), &["add", ".gitattributes", "filtered.txt"]);
    git(repo.path(), &["commit", "--quiet", "-m", "attributes"]);
    let marker = repo
        .path()
        .parent()
        .expect("parent")
        .join("project-code-ran");
    let filter = format!("sh -c 'printf owned > {}; cat'", marker.display());
    git(repo.path(), &["config", "filter.evil.smudge", &filter]);
    git(repo.path(), &["config", "filter.evil.clean", &filter]);
    let hook = repo.path().join(".git/hooks/post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nprintf owned > {}\n", marker.display()),
    )
    .expect("hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).expect("hook mode");

    let private = tempfile::tempdir().expect("private tempdir");
    let manager = isolation(repo.path(), private.path()).await;
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease");
    assert!(!marker.exists(), "project filter or hook executed");
    std::fs::write(lease.path().join("filtered.txt"), "changed\n").expect("change");
    let (usage, cost) = accounting();
    manager
        .collect(&lease, "done", usage, cost, CancellationToken::default())
        .await
        .expect("collect without project process execution");
    assert!(!marker.exists(), "project filter or hook executed");
}

#[test]
fn artifact_paths_and_text_are_bounded() {
    assert!(validate_relative_path(Path::new("--help;touch owned")).is_ok());
    assert!(validate_relative_path(Path::new("../escape")).is_err());
    assert!(validate_relative_path(Path::new("/absolute")).is_err());
    let (text, truncated) = truncate_utf8("hello 🐕", 8);
    assert_eq!(text, "hello ");
    assert!(truncated);
}
