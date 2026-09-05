use super::*;
#[cfg(unix)]
use crate::session_host::git::{
    parse_git_status, read_git_branch, resolve_git_executable,
    resolve_git_executable_for_caller_path, resolve_git_executable_from_candidates,
    run_bounded_git,
};
use crate::session_host::workspace::search_workspace;

#[cfg(unix)]
#[test]
fn workspace_search_honors_git_nested_and_tool_ignore_files_but_keeps_hidden_files() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(workspace.join(".git/info")).expect("git marker");
    fs::create_dir_all(workspace.join("ignored-dir")).expect("ignored directory");
    fs::create_dir_all(workspace.join("nested")).expect("nested directory");
    fs::write(
        workspace.join(".gitignore"),
        "ignored.txt\nignored-dir/*\nnested/*.rs\n",
    )
    .expect("gitignore");
    fs::write(
        workspace.join(".ignore"),
        "*.tmp\n!keep.tmp\n!ignored-dir/keep.rs\n",
    )
    .expect("tool ignore");
    fs::write(workspace.join(".git/info/exclude"), "info-excluded.rs\n").expect("git info exclude");
    fs::write(workspace.join("ignored.txt"), "ignored").expect("ignored file");
    fs::write(workspace.join("ignored-dir/secret.rs"), "ignored").expect("ignored child");
    fs::write(workspace.join("ignored-dir/keep.rs"), "visible").expect("kept child");
    fs::write(workspace.join("scratch.tmp"), "ignored").expect("tool ignored file");
    fs::write(workspace.join("keep.tmp"), "visible").expect("tool whitelist");
    fs::write(workspace.join("info-excluded.rs"), "ignored").expect("info ignored file");
    fs::write(workspace.join(".hidden.rs"), "visible").expect("hidden file");
    fs::write(workspace.join("nested/.gitignore"), "!visible.rs\n").expect("nested gitignore");
    fs::write(workspace.join("nested/nested-ignored.rs"), "ignored").expect("nested ignored file");
    fs::write(workspace.join("nested/visible.rs"), "visible").expect("visible file");
    fs::write(workspace.join(".git/HEAD"), "ref: refs/heads/main\n").expect("git internals");

    let (matches, truncated) = search_workspace(&workspace, "", 100).expect("search");
    assert!(!truncated);
    let paths = matches
        .into_iter()
        .map(|item| item.path)
        .collect::<BTreeSet<_>>();
    assert!(paths.contains(".hidden.rs"));
    assert!(paths.contains("keep.tmp"));
    assert!(paths.contains("nested/visible.rs"));
    assert!(!paths.contains("ignored.txt"));
    assert!(!paths.contains("ignored-dir/secret.rs"));
    assert!(!paths.contains("ignored-dir/keep.rs"));
    assert!(!paths.contains("scratch.tmp"));
    assert!(!paths.contains("info-excluded.rs"));
    assert!(!paths.contains("nested/nested-ignored.rs"));
    assert!(paths.iter().all(|path| !path.starts_with(".git/")));
}

#[cfg(unix)]
#[test]
fn workspace_search_keeps_fuzzy_reachable_candidates_and_deterministic_bounds() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(workspace.join("src")).expect("source directory");
    fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("main source");
    fs::write(workspace.join("alpha.rs"), "alpha\n").expect("alpha");
    fs::write(workspace.join("beta.rs"), "beta\n").expect("beta");

    let (fuzzy, truncated) = search_workspace(&workspace, "smr", 10).expect("fuzzy pool");
    assert!(!truncated);
    assert!(fuzzy.iter().any(|item| item.path == "src/main.rs"));

    let (bounded, truncated) =
        search_workspace(&workspace, "", 2).expect("bounded deterministic pool");
    assert!(truncated);
    assert_eq!(
        bounded
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>(),
        ["alpha.rs", "beta.rs"]
    );
}

#[cfg(unix)]
#[test]
fn workspace_search_supports_linked_git_worktree_excludes() {
    let root = tempdir().expect("root");
    let repository = root.path().join("repository");
    let workspace = root.path().join("linked-worktree");
    fs::create_dir(&repository).expect("repository");
    git(&repository, &["init", "--quiet"]);
    git(&repository, &["config", "user.name", "Rottweiler Test"]);
    git(
        &repository,
        &["config", "user.email", "test@example.invalid"],
    );
    fs::write(repository.join("tracked.rs"), "tracked\n").expect("tracked file");
    git(&repository, &["add", "tracked.rs"]);
    git(&repository, &["commit", "--quiet", "-m", "fixture"]);
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked-fixture",
            workspace.to_str().expect("UTF-8 worktree path"),
        ],
    );
    fs::create_dir_all(repository.join(".git/info")).expect("Git info directory");
    fs::write(repository.join(".git/info/exclude"), "excluded.rs\n").expect("common exclude");
    fs::write(workspace.join("excluded.rs"), "excluded\n").expect("excluded file");
    fs::write(workspace.join("visible.rs"), "visible\n").expect("visible file");

    let (matches, truncated) = search_workspace(&workspace, "", 100).expect("search");
    assert!(!truncated);
    let paths = matches
        .into_iter()
        .map(|item| item.path)
        .collect::<BTreeSet<_>>();
    assert!(paths.contains("visible.rs"));
    assert!(paths.contains("tracked.rs"));
    assert!(!paths.contains("excluded.rs"));
}

#[cfg(unix)]
#[test]
fn unsafe_ignore_controls_fail_closed_for_only_the_affected_subtree() {
    use std::os::unix::fs::symlink;

    for fixture in ["symlink", "oversized", "invalid-utf8"] {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(workspace.join("bad")).expect("bad subtree");
        fs::write(workspace.join("safe.rs"), "safe").expect("safe sibling");
        fs::write(workspace.join("bad/secret.rs"), "secret").expect("secret file");
        match fixture {
            "symlink" => {
                fs::write(root.path().join("outside-ignore"), "secret.rs\n")
                    .expect("outside ignore");
                symlink(
                    root.path().join("outside-ignore"),
                    workspace.join("bad/.gitignore"),
                )
                .expect("ignore symlink");
            }
            "oversized" => fs::write(
                workspace.join("bad/.gitignore"),
                vec![b'x'; MAX_IGNORE_FILE_BYTES + 1],
            )
            .expect("oversized ignore"),
            "invalid-utf8" => {
                fs::write(workspace.join("bad/.gitignore"), [0xff, b'\n']).expect("invalid ignore");
            }
            _ => unreachable!(),
        }

        let (matches, truncated) = search_workspace(&workspace, "", 100).expect("search");
        assert!(truncated, "{fixture}");
        let paths = matches
            .into_iter()
            .map(|item| item.path)
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("safe.rs"), "{fixture}");
        assert!(!paths.contains("bad"), "{fixture}");
        assert!(!paths.contains("bad/secret.rs"), "{fixture}");
    }
}

#[cfg(unix)]
#[test]
fn workspace_status_reports_modified_and_untracked_but_not_ignored_paths() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    git(&workspace, &["init", "--quiet"]);
    git(&workspace, &["config", "user.name", "Rottweiler Test"]);
    git(
        &workspace,
        &["config", "user.email", "test@example.invalid"],
    );
    fs::write(workspace.join(".gitignore"), "ignored.log\n").expect("gitignore");
    fs::write(workspace.join("tracked.rs"), "old\n").expect("tracked file");
    git(&workspace, &["add", ".gitignore", "tracked.rs"]);
    git(&workspace, &["commit", "--quiet", "-m", "fixture"]);
    fs::write(workspace.join("tracked.rs"), "new\n").expect("modified file");
    fs::write(workspace.join("untracked.rs"), "new\n").expect("untracked file");
    fs::write(workspace.join("ignored.log"), "ignored\n").expect("ignored file");

    let status = read_workspace_status(&workspace, "workspace".to_owned()).expect("status");
    assert!(!status.truncated);
    assert_eq!(status.changed_paths, ["tracked.rs", "untracked.rs"]);
    assert!(status.branch.is_some());

    fs::create_dir(workspace.join("nested")).expect("nested workspace");
    assert_eq!(
        read_git_branch(&workspace.join("nested")).expect("nested branch"),
        status.branch
    );

    let worktree = root.path().join("linked-worktree");
    git(
        &workspace,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked-branch",
            worktree.to_str().expect("worktree path"),
        ],
    );
    assert_eq!(
        read_git_branch(&worktree).expect("linked branch"),
        Some("linked-branch".to_owned())
    );

    git(&workspace, &["checkout", "--quiet", "--detach"]);
    let revision = Command::new("/usr/bin/git")
        .current_dir(&workspace)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .expect("detached revision");
    assert!(revision.status.success());
    assert_eq!(
        read_git_branch(&workspace).expect("detached branch"),
        Some(format!(
            "detached@{}",
            String::from_utf8(revision.stdout)
                .expect("UTF-8 revision")
                .trim()
        ))
    );
}

#[cfg(unix)]
#[test]
fn workspace_diff_covers_tracked_untracked_binary_ignored_and_truncated_files() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    git(&workspace, &["init", "--quiet"]);
    git(&workspace, &["config", "user.name", "Rottweiler Test"]);
    git(
        &workspace,
        &["config", "user.email", "test@example.invalid"],
    );
    fs::write(workspace.join(".gitignore"), "ignored.txt\n").expect("gitignore");
    fs::write(workspace.join("tracked.txt"), "old\n").expect("tracked text");
    fs::write(workspace.join("binary.bin"), [0, 1, 2]).expect("tracked binary");
    git(
        &workspace,
        &["add", ".gitignore", "tracked.txt", "binary.bin"],
    );
    git(&workspace, &["commit", "--quiet", "-m", "fixture"]);
    fs::write(workspace.join("tracked.txt"), "new\n").expect("modified text");
    fs::write(workspace.join("binary.bin"), [0, 1, 3]).expect("modified binary");
    fs::write(workspace.join("untracked.txt"), "hello\nworld\n").expect("untracked text");
    fs::write(workspace.join("ignored.txt"), "secret\n").expect("ignored text");
    fs::write(workspace.join("large.txt"), "line\n".repeat(1_000)).expect("large text");

    let tracked =
        read_workspace_diff(&workspace, Path::new("tracked.txt"), 8_192).expect("tracked diff");
    assert!(!tracked.binary);
    assert!(!tracked.truncated);
    assert!(tracked.unified_diff.contains("-old"));
    assert!(tracked.unified_diff.contains("+new"));

    let untracked =
        read_workspace_diff(&workspace, Path::new("untracked.txt"), 8_192).expect("untracked diff");
    assert!(!untracked.binary);
    assert!(untracked.unified_diff.contains("--- /dev/null"));
    assert!(untracked.unified_diff.contains("+hello"));

    let binary =
        read_workspace_diff(&workspace, Path::new("binary.bin"), 8_192).expect("binary diff");
    assert!(binary.binary);
    assert!(binary.unified_diff.contains("Binary files"));

    let ignored = read_workspace_diff(&workspace, Path::new("ignored.txt"), 8_192)
        .expect_err("ignored diff must fail closed");
    assert!(ignored.to_string().contains("Git-ignored"));

    let large = read_workspace_diff(&workspace, Path::new("large.txt"), 128).expect("bounded diff");
    assert!(large.truncated);
    assert!(large.unified_diff.len() <= 128);
}

#[cfg(unix)]
#[test]
fn porcelain_parser_keeps_rename_destination_and_rejects_unsafe_paths() {
    let (paths, truncated) = parse_git_status(
        b"R  new.rs\0old.rs\0?? nested/untracked.rs\0?? ../escape\0?? partial.rs",
        false,
    );
    assert!(truncated);
    assert_eq!(paths, ["nested/untracked.rs", "new.rs"]);
}

#[cfg(unix)]
#[test]
fn git_resolution_rejects_user_owned_executables_and_uses_a_system_identity() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir().expect("root");
    let hostile = root.path().join("git");
    fs::write(&hostile, "#!/bin/sh\nexit 0\n").expect("fake git");
    fs::set_permissions(&hostile, fs::Permissions::from_mode(0o700)).expect("fake git mode");

    assert!(resolve_git_executable_from_candidates([hostile.as_path()]).is_none());
    let hostile_path = std::env::join_paths([root.path()]).expect("hostile PATH");
    let resolved_for_hostile_path = resolve_git_executable_for_caller_path(Some(&hostile_path))
        .expect("system Git with hostile PATH");
    let system = resolve_git_executable(root.path()).expect("system Git identity");
    assert_eq!(resolved_for_hostile_path, system);
    assert_ne!(
        system,
        fs::canonicalize(hostile).expect("canonical hostile executable")
    );
    assert!(system.starts_with("/usr/bin") || system.starts_with("/bin"));
}

#[cfg(unix)]
#[test]
fn bounded_git_kills_descendants_that_keep_stdout_open() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let git = root.path().join("fake-git");
    fs::write(
        &git,
        "#!/bin/sh\nsleep 5 &\nprintf '?? held.rs\\0'\nexit 0\n",
    )
    .expect("fake git");
    fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).expect("fake git mode");
    let started = Instant::now();
    for _ in 0..4 {
        let output = run_bounded_git(&git, &workspace, &[], 1024, Duration::from_secs(2))
            .expect("caller-owned drain retains output across descendant teardown");
        assert_eq!(output.stdout, b"?? held.rs\0");
        assert!(!output.overflow);
    }
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn workspace_preview_fails_closed_for_traversal_symlink_and_binary_without_path_leakage() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join("safe.txt"), "safe").expect("safe file");
    fs::write(workspace.join("binary.bin"), [0, 1, 2]).expect("binary file");
    #[cfg(unix)]
    std::os::unix::fs::symlink("safe.txt", workspace.join("link.txt")).expect("symlink");

    for path in ["../safe.txt", "/etc/passwd"] {
        let error = safe_relative_path(path).expect_err("unsafe relative path");
        assert!(!error.to_string().contains(&workspace.display().to_string()));
    }
    assert_eq!(
        safe_relative_path("nested//safe.txt").expect("normalized path"),
        Path::new("nested/safe.txt")
    );
    assert_eq!(
        split_virtual_path("@root/2/nested/safe.txt").expect("virtual path"),
        (2, PathBuf::from("nested/safe.txt"))
    );
    for path in ["@root/0/file", "@root/1", "@root/1/../escape"] {
        assert!(split_virtual_path(path).is_err(), "{path}");
    }
    for path in ["binary.bin", "link.txt"] {
        let relative = safe_relative_path(path).expect("normalized path");
        let error = preview_file(&workspace, &relative, 1024).expect_err("unsafe preview");
        assert!(!error.to_string().contains(&workspace.display().to_string()));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_preview_rejects_before_opening_under_one_hundred_milliseconds() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let fifo = workspace.join("blocked.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo fixture")
            .success()
    );
    let started = Instant::now();
    preview_file(&workspace, Path::new("blocked.fifo"), 1024).expect_err("FIFO must fail");
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[cfg(unix)]
#[test]
fn descriptor_relative_queries_do_not_escape_during_directory_swap_race() {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    let swap = workspace.join("swap");
    let held = workspace.join("held");
    fs::create_dir_all(&swap).expect("safe directory");
    fs::create_dir_all(&outside).expect("outside directory");
    fs::write(swap.join("target.txt"), "SAFE").expect("safe file");
    fs::write(outside.join("target.txt"), "OUTSIDE_CANARY").expect("outside file");
    fs::write(outside.join("OUTSIDE_CANARY.txt"), "outside").expect("outside marker");

    let running = Arc::new(AtomicBool::new(true));
    let attacker_running = Arc::clone(&running);
    let attacker_swap = swap.clone();
    let attacker_held = held.clone();
    let attacker_outside = outside.clone();
    let attacker = thread::spawn(move || {
        while attacker_running.load(Ordering::Relaxed) {
            if fs::rename(&attacker_swap, &attacker_held).is_ok() {
                std::os::unix::fs::symlink(&attacker_outside, &attacker_swap)
                    .expect("race symlink");
                fs::remove_file(&attacker_swap).expect("remove race symlink");
                fs::rename(&attacker_held, &attacker_swap).expect("restore safe directory");
            }
            thread::yield_now();
        }
    });

    for _ in 0..250 {
        if let Ok(preview) = preview_file(&workspace, Path::new("swap/target.txt"), 1024) {
            assert_eq!(
                preview.data,
                AttachmentData::Text {
                    content: "SAFE".to_owned()
                }
            );
        }
        if let Ok((matches, _)) = search_workspace(&workspace, "OUTSIDE_CANARY", 10) {
            assert!(matches.is_empty(), "search escaped through a raced symlink");
        }
    }
    running.store(false, Ordering::Relaxed);
    attacker.join().expect("attacker thread");

    let preview = preview_file(&workspace, Path::new("swap/target.txt"), 1024)
        .expect("safe directory restored");
    assert_eq!(
        preview.data,
        AttachmentData::Text {
            content: "SAFE".to_owned()
        }
    );
}
