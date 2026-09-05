#![allow(clippy::expect_used)]

#[cfg(target_os = "linux")]
mod linux {
    use rw_sandbox::{
        PreparationExecutable, PreparationFilesystem, SandboxPolicy, SandboxSupport,
        maybe_run_helper, probe, shell_launch_plan,
    };
    use std::{ffi::OsString, fs, path::Path, process::Command};
    pub(super) fn run() {
        if maybe_run_helper(std::env::args_os()).expect("sandbox helper dispatch") {
            unreachable!("sandbox helper replaces the process")
        }
        if std::env::var_os("RW_PREPARATION_DRIVER_CHILD").is_none() {
            fixture();
            return;
        }
        let root = std::path::PathBuf::from(
            std::env::var_os("RW_PREPARATION_DRIVER_CHILD").expect("root"),
        );
        let shell = fs::canonicalize("/bin/sh").expect("shell path");
        let layout = PreparationFilesystem::new(
            &root.join("code"),
            &root.join("work"),
            &root.join("mount"),
            Some(&root.join("output")),
            PreparationExecutable::capture(&shell).expect("shell identity"),
        )
        .expect("declared view");
        let policy = SandboxPolicy::for_preparation(layout).expect("preparation policy");
        let script = r#"set -eu
    test -r /plugin/entry.ts
    test ! -e /plugin/.ssh
    test ! -e /plugin/alias/secret
    test ! -e /root
    test ! -e /home
    test ! -e /workspace
    test ! -e /etc/shadow
    if printf changed >> /plugin/entry.ts 2>/dev/null; then exit 31; fi
    printf working > /scratch/work
    printf prepared > /output/result
    if mount -t tmpfs tmpfs /scratch 2>/dev/null; then exit 32; fi
    test "$(sed -n 's/^CapEff:[[:space:]]*//p' /proc/self/status)" = 0000000000000000
    test "$(sed -n 's/^CapPrm:[[:space:]]*//p' /proc/self/status)" = 0000000000000000
    for fd in /proc/self/fd/*; do
      target=$(readlink "$fd" || true)
      case "$target" in *rw-preparation*|*/mount|*/code|*/work) exit 33 ;; esac
    done
    printf 'production preparation view boundaries pass\n'
    "#;
        let args = [OsString::from("-c"), OsString::from(script)];
        let helper = std::env::current_exe().expect("helper");
        let plan = shell_launch_plan(&policy, &helper, &shell, &args).expect("view launch plan");
        let status = Command::new(&plan.program)
            .args(&plan.args)
            .status()
            .expect("sandbox process");
        assert!(status.success(), "source view failed: {status}");
        compiler(&root, &helper);
        replacement_is_rejected(&root, &helper);
    }
    fn fixture() {
        let capability = probe();
        if capability.support != SandboxSupport::Enforced {
            assert!(
                std::env::var_os("ROTTWEILER_REQUIRE_LINUX_SANDBOX").is_none(),
                "required Linux sandbox unavailable: {capability:?}"
            );
            eprintln!("skipping preparation view acceptance: {capability:?}");
            return;
        }
        let root = tempfile::tempdir().expect("fixture");
        for name in ["code", "work", "mount", "output"] {
            fs::create_dir(root.path().join(name)).expect("fixture directory");
        }
        let home = root.path().join("code");
        fs::write(
            home.join("package.json"),
            r#"{"name":"preparation-probe","type":"module"}"#,
        )
        .expect("package");
        fs::write(home.join("entry.ts"), "export const answer = 42;").expect("entry");
        fs::create_dir(home.join(".ssh")).expect("credential directory");
        fs::write(home.join(".ssh/secret"), "must remain private").expect("secret");
        std::os::unix::fs::symlink(".ssh", home.join("alias")).expect("credential alias");
        let status = Command::new(std::env::current_exe().expect("driver"))
            .env("RW_PREPARATION_DRIVER_CHILD", root.path())
            .env("HOME", home)
            .status()
            .expect("view driver");
        assert!(status.success(), "preparation driver failed: {status}");
        assert_eq!(
            fs::read(root.path().join("output/result")).expect("published result"),
            b"prepared"
        );
    }
    fn replacement_is_rejected(root: &Path, helper: &Path) {
        use std::io::{Seek as _, Write as _};
        let program = root.join("approved-host");
        fs::copy("/bin/true", &program).expect("fixture program");
        let identity = PreparationExecutable::capture(&program).expect("approved program");
        let mount = root.join("replacement-view");
        fs::create_dir(&mount).expect("replacement mount");
        let layout = PreparationFilesystem::new(
            &root.join("code"),
            &root.join("work"),
            &mount,
            None,
            identity,
        )
        .expect("pinned layout");
        let policy = SandboxPolicy::for_preparation(layout).expect("policy");
        let previous = root.join("previous-host");
        fs::rename(&program, &previous).expect("retain approved inode");
        fs::copy("/bin/true", &program).expect("replace executable inode");
        let rejected = || {
            let plan = shell_launch_plan(&policy, helper, &program, &[]).expect("launch plan");
            let output = Command::new(&plan.program)
                .args(&plan.args)
                .output()
                .expect("rejected launch");
            assert!(!output.status.success(), "substituted executable ran");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("UntrustedHelper"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        rejected();
        fs::rename(&previous, &program).expect("restore approved inode");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&program)
            .expect("fixture writable program");
        file.seek(std::io::SeekFrom::Start(0)).expect("first byte");
        file.write_all(b"!").expect("change approved content");
        file.sync_all().expect("flush changed byte");
        rejected();
        println!("production executable replacement and content changes rejected");
    }
    fn compiler(root: &Path, helper: &Path) {
        if let Some(host) = std::env::var_os("ROTTWEILER_PREPARATION_TEST_HOST") {
            let mut reports = Vec::new();
            for operation in ["graph", "bundle"] {
                let mount = root.join(format!("{operation}-view"));
                fs::create_dir(&mount).expect("operation view");
                let layout = PreparationFilesystem::new(
                    &root.join("code"),
                    &root.join("work"),
                    &mount,
                    Some(&root.join("output")),
                    PreparationExecutable::capture(Path::new(&host)).expect("host identity"),
                )
                .expect("compiler view");
                let policy = SandboxPolicy::for_preparation(layout).expect("compiler policy");
                let mut args = vec![
                    OsString::from(operation),
                    root.join("code").into_os_string(),
                    root.join("code/entry.ts").into_os_string(),
                ];
                if operation == "bundle" {
                    args.push(root.join("output").into_os_string());
                }
                let plan = shell_launch_plan(&policy, helper, Path::new(&host), &args)
                    .expect("compiler plan");
                let output = Command::new(&plan.program)
                    .args(&plan.args)
                    .output()
                    .expect("compiler output");
                assert!(
                    output.status.success(),
                    "{operation} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                reports.push(
                    serde_json::from_slice::<serde_json::Value>(&output.stdout)
                        .expect("graph report"),
                );
            }
            assert_eq!(reports[0], reports[1]);
            assert!(root.join("output/plugin.mjs").is_file());
            println!("production compiler graph and bundle pass: {}", reports[0]);
        }
    }
}
#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}
#[cfg(not(target_os = "linux"))]
fn main() {}
