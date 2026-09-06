use super::*;

#[cfg(target_os = "linux")]
#[test]
fn linux_stat_parser_extracts_process_group_and_zombie_state() {
    assert_eq!(
        parse_linux_process_stat(b"123 (worker (fixture)) Z 1 42 42 0 -1\n"),
        Some((42, b'Z'))
    );
    assert_eq!(
        parse_linux_process_stat(b"123 (worker) S 1 42 42 0 -1\n"),
        Some((42, b'S'))
    );
    assert_eq!(parse_linux_process_stat(b"123 worker Z 1 42\n"), None);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_group_status_accepts_only_absent_or_all_zombie_members() {
    assert_eq!(
        parse_macos_process_group_status(b"7 Ss\n42 Z\n42 Z+\n", 42),
        Some(true)
    );
    assert_eq!(
        parse_macos_process_group_status(b"7 Ss\n9 R\n", 42),
        Some(true)
    );
    assert_eq!(
        parse_macos_process_group_status(b"42 Z\n42 S\n", 42),
        Some(false)
    );
    assert_eq!(parse_macos_process_group_status(b"42\n", 42), None);
    assert_eq!(parse_macos_process_group_status(b"", 42), None);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn fake_eperm_with_a_live_group_remains_fail_closed() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let mut command = Command::new("/bin/sleep");
    command
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().expect("live process-group fixture");
    let process_group = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .expect("fixture process group");
    assert!(
        !macos_terminal_group_probe(rustix::io::Errno::PERM, process_group).await,
        "EPERM with a demonstrably live member must remain fail-closed"
    );
    terminate_process_group(child.id());
    child.wait().await.expect("reap live process-group fixture");
}

#[cfg(unix)]
#[test]
fn nonblocking_execution_lease_refuses_an_existing_owner_immediately() {
    let root = tempdir().expect("temp directory");
    let path = root.path().join("execution.lock");
    let _owner = ExecutionLease::acquire(&path).expect("initial execution lease");
    let started = std::time::Instant::now();
    let error = ExecutionLease::try_acquire(&path).expect_err("second lease must fail");
    assert!(started.elapsed() < Duration::from_millis(100));
    assert!(
        matches!(error, ToolError::Io { source, .. } if source.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[cfg(unix)]
#[test]
fn recovery_execution_lease_wait_is_bounded() {
    let root = tempdir().expect("temp directory");
    let path = root.path().join("execution.lock");
    let _owner = ExecutionLease::acquire(&path).expect("initial execution lease");
    let started = std::time::Instant::now();
    let error = ExecutionLease::acquire_for(&path, Duration::from_millis(20))
        .expect_err("recovery lease must time out");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(error.to_string().contains("recovery timeout"));
}

#[test]
fn built_in_safe_list_accepts_hardened_read_only_commands() {
    for command in [
        "git status",
        "git status --short",
        "git status --porcelain=v1 -- .",
        "git diff",
        "git diff --stat -- .",
        "cat Cargo.toml",
        "cat -- Cargo.toml",
        "ls",
        "ls -la crates",
        "cat Cargo.toml && git diff --stat; ls crates",
    ] {
        assert_eq!(
            classify_safe_command(command),
            CommandSafety::SafeListed,
            "expected safe-list classification for {command}"
        );
    }
    if audited_bat().is_some() {
        for command in ["bat README.md", "bat -n --color=always src/lib.rs"] {
            assert_eq!(
                classify_safe_command(command),
                CommandSafety::SafeListed,
                "expected installed bat to use the read-only safe path"
            );
        }
    }
    for command in [
        "git clean -fd",
        "git status && rm -rf /tmp/example",
        "git status; curl https://example.invalid",
        "git status $(touch escaped)",
        "git status `touch escaped`",
        "git -c alias.status='!touch escaped' status",
        "/usr/bin/git status",
        "./git status",
        "evil/git status",
        "PATH=. git status",
        "env PATH=. git status",
        "git status --help",
        "git diff --output=leak.patch",
        "git diff --ext-diff",
        "git diff --textconv",
        "git diff --no-index first second",
        "cat Cargo.toml > copy",
        "ls | tee listing",
        "/bin/cat Cargo.toml",
        "PATH=. cat Cargo.toml",
        "bat --pager='sh -c touch${IFS}escaped' README.md",
        "bat --paging=always README.md",
        "bat --diff README.md",
        "bat --config-file=project.conf README.md",
        "sh -c 'git status'",
        "",
    ] {
        assert_eq!(
            classify_safe_command(command),
            CommandSafety::RequiresApproval,
            "expected approval classification for {command}"
        );
    }
    assert!(!safe_bat_arguments(&["--pager=less".to_owned()]));
    assert!(safe_bat_arguments(&[
        "--color=always".to_owned(),
        "README.md".to_owned(),
    ]));
}

#[cfg(unix)]
#[tokio::test]
async fn safe_cat_uses_the_audited_binary_and_ignores_caller_environment() {
    use std::os::unix::fs::PermissionsExt as _;

    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temporary directory");
    let malicious = root.path().join("malicious");
    std::fs::create_dir(&malicious).expect("malicious directory");
    let marker = root.path().join("workspace-cat-ran");
    let fake_cat = malicious.join("cat");
    std::fs::write(
        &fake_cat,
        format!(
            "#!/bin/sh\ntouch '{}'\nprintf MALICIOUS\n",
            marker.display()
        ),
    )
    .expect("fake cat");
    std::fs::set_permissions(&fake_cat, std::fs::Permissions::from_mode(0o755))
        .expect("fake cat mode");
    std::fs::write(root.path().join("input.txt"), "expected output\n").expect("input");

    let sink = Arc::new(RecordingSink::default());
    let result = TokioCommandExecutor::default()
        .run(
            CommandRequest {
                command: "cat input.txt".to_owned(),
                cwd: root.path().to_path_buf(),
                env: BTreeMap::from([
                    ("PATH".to_owned(), malicious.display().to_string()),
                    ("BASH_ENV".to_owned(), fake_cat.display().to_string()),
                    ("GIT_CONFIG_COUNT".to_owned(), "999".to_owned()),
                ]),
                network_domains: Vec::new(),
                sandbox: BashSandboxMode::Sandboxed,
            },
            CancellationToken::default(),
            sink.clone(),
        )
        .await
        .expect("safe cat");
    assert_eq!(result.exit_code, 0);
    assert!(!marker.exists(), "workspace-controlled cat was executed");
    let output = sink
        .0
        .lock()
        .expect("sink")
        .iter()
        .map(|chunk| chunk.content.as_str())
        .collect::<String>();
    assert_eq!(output, "expected output\n");
}

#[test]
fn configured_safe_list_requires_every_conservative_compound_segment() {
    let classifier =
        CommandSafetyClassifier::new(&["cargo test*".to_owned()]).expect("configured classifier");
    for command in [
        "cargo test",
        "cargo test --workspace",
        "cargo test && cargo test --doc",
        "cargo test; cargo test --lib",
    ] {
        assert_eq!(classifier.classify(command), CommandSafety::SafeListed);
    }
    for command in [
        "cargo test && rm -rf target",
        "cargo test | tee output",
        "cargo test > output",
        "cargo test $(touch escaped)",
        "cargo test && 'unterminated",
    ] {
        assert_eq!(
            classifier.classify(command),
            CommandSafety::RequiresApproval,
            "unsafe compound was auto-safe: {command}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn general_commands_do_not_source_login_or_shell_control_profiles() {
    use std::os::unix::fs::PermissionsExt as _;

    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("root");
    let home = root.path().join("home");
    let trusted = root.path().join("trusted");
    let malicious = root.path().join("malicious");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&trusted).expect("trusted");
    std::fs::create_dir_all(&malicious).expect("malicious");
    let profile_canary = root.path().join("profile-ran");
    let result = root.path().join("result");
    std::fs::write(
        home.join(".profile"),
        format!(
            "printf profile > '{}'; export PATH='{}'\n",
            profile_canary.display(),
            malicious.display()
        ),
    )
    .expect("profile");
    for (directory, value) in [(&trusted, "trusted"), (&malicious, "malicious")] {
        let executable = directory.join("identity-probe");
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf {value} > \"$RESULT\"\n"),
        )
        .expect("probe");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("probe mode");
    }
    let outcome = TokioCommandExecutor::default()
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                command: "identity-probe".to_owned(),
                cwd: root.path().to_path_buf(),
                env: BTreeMap::from([
                    ("HOME".to_owned(), home.display().to_string()),
                    (
                        "PATH".to_owned(),
                        format!("{}:/usr/bin:/bin", trusted.display()),
                    ),
                    ("RESULT".to_owned(), result.display().to_string()),
                    (
                        "BASH_ENV".to_owned(),
                        home.join(".profile").display().to_string(),
                    ),
                    (
                        "ENV".to_owned(),
                        home.join(".profile").display().to_string(),
                    ),
                ]),
                network_domains: Vec::new(),
            },
            CancellationToken::default(),
            Arc::new(crate::NoopOutputSink),
        )
        .await
        .expect("command");
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(std::fs::read_to_string(result).expect("result"), "trusted");
    assert!(!profile_canary.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_rejects_process_group_escape_launchers() {
    for command in [
        "setsid sh -c true",
        "/usr/bin/nohup true",
        "daemon --name canary",
        "unterminated '",
    ] {
        assert!(command_can_escape_process_group(command), "{command}");
    }
    assert!(!command_can_escape_process_group("printf ordinary"));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandboxed_eperm_and_explicit_unsandboxed_escape_have_distinct_boundaries() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&scratch).expect("scratch");
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).expect("outside directory");
    std::fs::write(outside.join("canary"), "blocked").expect("outside canary");
    let policy = Arc::new(
        SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
            .expect("sandbox policy"),
    );
    let executor = TokioCommandExecutor::default()
        .sandboxed(policy, crate::test_support::sandbox_helper())
        .with_policy_egress(true);
    let sink = Arc::new(RecordingSink::default());
    let command = format!(
        "printf allowed > allowed; rm -rf {}",
        shell_words::quote(&outside.to_string_lossy())
    );
    let outcome = executor
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command,
                cwd: workspace.clone(),
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            sink.clone(),
        )
        .await
        .expect("guarded command outcome");
    assert_ne!(outcome.exit_code, 0);
    assert_eq!(
        std::fs::read_to_string(workspace.join("allowed")).expect("allowed write"),
        "allowed"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("canary")).expect("blocked outside canary"),
        "blocked"
    );
    let stderr = sink
        .0
        .lock()
        .expect("sink")
        .iter()
        .filter(|chunk| chunk.stream == ToolOutputStream::Stderr)
        .map(|chunk| chunk.content.as_str())
        .collect::<String>();
    assert!(
        stderr.contains("Operation not permitted"),
        "expected EPERM diagnostic, got {stderr:?}"
    );

    std::fs::remove_dir_all(&outside).expect("clean blocked canary");

    let escaped = executor
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Unsandboxed,
                network_domains: Vec::new(),
                command: format!("printf approved > '{}'", outside.display()),
                cwd: workspace,
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("explicit unsandboxed command");
    assert_eq!(escaped.exit_code, 0);
    assert_eq!(
        std::fs::read_to_string(&outside).expect("outside canary"),
        "approved"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandboxed_executor_denies_network_even_for_safe_list_eligible_processes() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&scratch).expect("scratch");
    let policy = Arc::new(
        SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
            .expect("sandbox policy"),
    );
    let probe = workspace.join("network-denial-probe.py");
    std::fs::write(
        &probe,
        r#"import errno, os, socket, sys
if any(os.environ.get(k) for k in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy")):
    sys.exit(94)
s = socket.socket()
try:
    s.connect(("127.0.0.1", 9))
except OSError as error:
    sys.exit(0 if error.errno in (errno.EPERM, errno.EACCES) else 93)
sys.exit(92)
"#,
    )
    .expect("network denial probe");
    let command = format!("python3 {}", shell_words::quote(&probe.to_string_lossy()));
    let classifier = Arc::new(
        CommandSafetyClassifier::new(&[globset::escape(&command)])
            .expect("test safe-list classifier"),
    );
    let executor = TokioCommandExecutor::default()
        .sandboxed(policy, crate::test_support::sandbox_helper())
        .with_command_safety(Arc::clone(&classifier))
        .with_policy_egress(true);
    assert_eq!(classifier.classify(&command), CommandSafety::SafeListed);
    let outcome = executor
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command,
                cwd: workspace,
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("guarded command outcome");
    assert_eq!(outcome.exit_code, 0, "network denial probe must see EPERM");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn requested_domains_receive_one_command_scoped_proxy_only() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&scratch).expect("scratch");
    let lifecycles = Arc::new(Mutex::new(Vec::new()));
    let executor = TokioCommandExecutor::default()
        .sandboxed(
            Arc::new(
                SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
                    .expect("sandbox policy"),
            ),
            crate::test_support::sandbox_helper(),
        )
        .with_policy_egress(true)
        .with_proxy_lifecycle_observer(Arc::clone(&lifecycles));
    let sink = Arc::new(RecordingSink::default());
    let outcome = executor
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: vec!["example.com".to_owned()],
                command: "printf '%s' \"$HTTPS_PROXY\"".to_owned(),
                cwd: workspace,
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            sink.clone(),
        )
        .await
        .expect("network-scoped command");
    assert_eq!(outcome.exit_code, 0);
    let output = sink
        .0
        .lock()
        .expect("sink")
        .iter()
        .filter(|chunk| chunk.stream == ToolOutputStream::Stdout)
        .map(|chunk| chunk.content.as_str())
        .collect::<String>();
    let _proxy = url::Url::parse(&output).expect("proxy URL");
    let observed = lifecycles.lock().expect("lifecycle observer");
    assert_eq!(observed.len(), 1);
    assert!(
        observed[0].is_stopped(),
        "per-command proxy listener supervisors were not joined"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn safe_listed_git_status_really_runs_inside_the_sandbox() {
    use std::os::unix::fs::PermissionsExt as _;

    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&scratch).expect("scratch");
    let git = audited_system_git().expect("audited system git");
    assert!(
        std::process::Command::new(git)
            .args(["init", "--quiet"])
            .current_dir(&workspace)
            .status()
            .expect("git init")
            .success()
    );
    let malicious_git = workspace.join("git");
    let executed = workspace.join("malicious-git-executed");
    std::fs::write(
        &malicious_git,
        format!(
            "#!/bin/sh\nprintf HOST_SECRET_CANARY\ntouch '{}'\n",
            executed.display()
        ),
    )
    .expect("malicious workspace git");
    std::fs::set_permissions(&malicious_git, std::fs::Permissions::from_mode(0o755))
        .expect("malicious git executable mode");
    assert!(
        std::process::Command::new(git)
            .args(["config", "core.fsmonitor", "./git"])
            .current_dir(&workspace)
            .status()
            .expect("malicious local git config")
            .success()
    );
    assert_eq!(
        classify_safe_command("git status --short"),
        CommandSafety::SafeListed
    );
    let executor = TokioCommandExecutor::default().sandboxed(
        Arc::new(
            SandboxPolicy::new([&workspace, &scratch], rw_sandbox::NetworkPolicy::Deny)
                .expect("sandbox policy"),
        ),
        crate::test_support::sandbox_helper(),
    );
    let sink = Arc::new(RecordingSink::default());
    let outcome = executor
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: "git status --short".to_owned(),
                cwd: workspace.clone(),
                env: BTreeMap::from([
                    ("PATH".to_owned(), workspace.display().to_string()),
                    ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
                    ("GIT_CONFIG_KEY_0".to_owned(), "core.fsmonitor".to_owned()),
                    ("GIT_CONFIG_VALUE_0".to_owned(), "./git".to_owned()),
                    ("BASH_ENV".to_owned(), malicious_git.display().to_string()),
                    ("ENV".to_owned(), malicious_git.display().to_string()),
                ]),
            },
            CancellationToken::default(),
            sink.clone(),
        )
        .await
        .expect("sandboxed git status");
    assert_eq!(outcome.exit_code, 0);
    assert!(!executed.exists(), "workspace-controlled git was executed");
    let output = sink
        .0
        .lock()
        .expect("sink")
        .iter()
        .map(|chunk| chunk.content.as_str())
        .collect::<String>();
    assert!(!output.contains("HOST_SECRET_CANARY"), "{output:?}");
}
