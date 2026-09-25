//! Read-only git forms run without a prompt; every writing, program-launching
//! or network form still requires approval.
use super::*;

#[test]
fn read_only_git_forms_and_their_compounds_are_safe_listed() {
    if audited_system_git().is_none() {
        return;
    }
    for command in [
        "git log --oneline -20 -- calc.py && git status --short",
        "git log --oneline -20 -- calc.py",
        "git log -p --stat --format='%h %an %s' main..HEAD",
        "git show HEAD~1 --stat",
        "git show HEAD:calc.py",
        "git blame -L 1,20 calc.py",
        "git branch",
        "git branch -a",
        "git branch -av",
        "git branch --show-current",
        "git branch --list 'feature/*'",
        "git branch --merged main",
        "git branch --sort=-committerdate --format='%(refname:short)'",
        "git rev-parse --show-toplevel",
        "git rev-parse --abbrev-ref HEAD",
        "git ls-files -- '*.py'",
        "git remote",
        "git remote -v",
        "git remote get-url origin",
        "git describe --tags --always",
        "git shortlog -sn HEAD",
        "git reflog",
        "git reflog show -n 5 HEAD",
        "git grep -n 'def add' -- '*.py'",
        "git config --get user.name",
        "git config --list --show-origin",
        "git config --get-regexp '^remote\\.'",
        "git config get user.email",
        "git config list --local",
        "git diff --stat && git log -3 --oneline; git status",
    ] {
        assert_eq!(
            classify_safe_command(command),
            CommandSafety::SafeListed,
            "expected safe-list classification for {command}"
        );
    }
}

#[test]
fn writing_launching_and_network_git_forms_require_approval() {
    for command in [
        // Global configuration and repository selection precede the subcommand.
        "git -c core.pager=less log",
        "git -C /tmp log",
        "git --no-pager log",
        "git --git-dir=/tmp/x log",
        // Output files, external diff, textconv, signatures and help viewers.
        "git log --output=leak.patch",
        "git log --output leak.patch",
        "git show --ext-diff",
        "git log -p --textconv",
        "git log --show-signature",
        "git log --format='%h %G?'",
        "git show --pretty=format:%GS",
        "git diff --no-index a b",
        "git log --help",
        "git blame --textconv calc.py",
        "git grep -O foo",
        "git grep -Oless foo",
        "git grep -nO foo",
        "git grep --open-files-in-pager=less foo",
        "git grep --textconv foo",
        // Branch mutation and creation.
        "git branch feature",
        "git branch -v feature",
        "git branch -d feature",
        "git branch -D feature",
        "git branch -m old new",
        "git branch -c old new",
        "git branch -u origin/main",
        "git branch --set-upstream-to=origin/main",
        "git branch --edit-description",
        "git branch --format='%(signature)'",
        // Remote and network forms.
        "git remote show origin",
        "git remote add evil https://example.invalid/repo",
        "git remote set-url origin https://example.invalid/repo",
        "git remote update",
        "git fetch",
        "git pull",
        "git push",
        // Reflog maintenance, index refresh and stdin readers.
        "git reflog expire --all",
        "git reflog delete HEAD@{1}",
        "git reflog HEAD",
        "git describe --dirty",
        "git describe --broken",
        "git rev-parse --parseopt",
        // Config set, unset and edit forms.
        "git config user.name evil",
        "git config user.name",
        "git config --add user.name evil",
        "git config --unset user.name",
        "git config --replace-all user.name evil",
        "git config -e",
        "git config --get user.name --list",
        "git config --get",
        "git config set user.name evil",
        "git config unset user.name",
        "git config edit",
        "git config get",
        // Unlisted subcommands.
        "git checkout main",
        "git stash list",
        "git log | less",
        "git log > out.txt",
    ] {
        assert_eq!(
            classify_safe_command(command),
            CommandSafety::RequiresApproval,
            "expected approval classification for {command}"
        );
    }
}

#[test]
fn hardened_git_argv_disables_repository_selected_programs() {
    let Some(git) = audited_system_git() else {
        return;
    };
    let argv = hardened_git_argv("git log -p --oneline -- calc.py").expect("hardened log");
    assert_eq!(argv[0], git.to_string_lossy());
    let settings = argv
        .windows(2)
        .filter(|pair| pair[0] == "-c")
        .map(|pair| pair[1].as_str())
        .collect::<Vec<_>>();
    for setting in [
        "core.fsmonitor=false",
        "core.hooksPath=/dev/null",
        "diff.external=",
        "log.showSignature=false",
        "gpg.program=false",
        "pager.log=false",
    ] {
        assert!(settings.contains(&setting), "missing {setting} in {argv:?}");
    }
    let log = argv
        .iter()
        .position(|argument| argument == "log")
        .expect("subcommand");
    assert_eq!(
        &argv[log..],
        [
            "log",
            "--no-ext-diff",
            "--no-textconv",
            "-p",
            "--oneline",
            "--",
            "calc.py"
        ]
    );
    let reflog = hardened_git_argv("git reflog show -n 3").expect("hardened reflog");
    let start = reflog
        .iter()
        .position(|argument| argument == "reflog")
        .expect("reflog");
    assert_eq!(
        &reflog[start..],
        [
            "reflog",
            "show",
            "--no-ext-diff",
            "--no-textconv",
            "-n",
            "3"
        ]
    );
    let bare = hardened_git_argv("git reflog").expect("hardened bare reflog");
    assert!(bare.ends_with(&[
        "reflog".to_owned(),
        "show".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned()
    ]));
    let blame = hardened_git_argv("git blame calc.py").expect("hardened blame");
    assert!(blame.ends_with(&[
        "blame".to_owned(),
        "--no-textconv".to_owned(),
        "calc.py".to_owned()
    ]));
    assert!(hardened_git_argv("git branch -d feature").is_none());
    assert!(hardened_git_argv("git config user.name evil").is_none());
    let compound = hardened_safe_compound("git log --oneline -20 -- calc.py && git status --short")
        .expect("hardened compound");
    assert!(compound.contains("--no-textconv"));
    assert!(compound.contains(" && "));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn safe_listed_git_history_never_runs_repository_selected_programs() {
    use std::os::unix::fs::PermissionsExt as _;

    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    let scratch_owner = crate::CommandScratch::create("fixture").expect("scratch owner");
    let scratch = scratch_owner.path().to_path_buf();
    std::fs::create_dir(&workspace).expect("workspace");
    let git = audited_system_git().expect("audited system git");
    let run_git = |args: &[&str]| {
        assert!(
            std::process::Command::new(git)
                .args(args)
                .current_dir(&workspace)
                .status()
                .expect("git setup")
                .success(),
            "git {args:?}"
        );
    };
    run_git(&["init", "--quiet"]);
    let program = workspace.join("convert");
    let executed = workspace.join("repository-program-executed");
    std::fs::write(
        &program,
        format!("#!/bin/sh\ntouch '{}'\ncat \"$1\"\n", executed.display()),
    )
    .expect("repository-selected program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("program mode");
    std::fs::write(workspace.join(".gitattributes"), "*.py diff=evil\n").expect("attributes");
    std::fs::write(
        workspace.join("calc.py"),
        "def add(a, b):\n    return a + b\n",
    )
    .expect("source");
    run_git(&["add", "."]);
    run_git(&[
        "-c",
        "user.name=fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "init",
    ]);
    // Repository configuration selects the program only after setup.
    run_git(&["config", "diff.evil.textconv", "./convert"]);
    run_git(&["config", "core.fsmonitor", "./convert"]);
    run_git(&["config", "gpg.program", "./convert"]);
    run_git(&["config", "log.showSignature", "true"]);
    let command = "git log -p --oneline -- calc.py && git show HEAD && git blame calc.py && git status --short";
    assert_eq!(classify_safe_command(command), CommandSafety::SafeListed);
    let executor = TokioCommandExecutor::default().sandboxed(
        Arc::new(
            SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
                .expect("sandbox policy"),
        ),
        crate::test_support::sandbox_helper(),
        scratch_owner,
    );
    let sink = Arc::new(RecordingSink::default());
    let outcome = executor
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: command.to_owned(),
                cwd: workspace.clone(),
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            sink.clone(),
        )
        .await
        .expect("sandboxed git history");
    let output = sink
        .0
        .lock()
        .expect("sink")
        .iter()
        .map(|chunk| chunk.content.as_str())
        .collect::<String>();
    assert_eq!(outcome.exit_code, 0, "{output}");
    assert!(output.contains("return a + b"), "{output}");
    assert!(
        !executed.exists(),
        "a repository-selected program was executed"
    );
}
