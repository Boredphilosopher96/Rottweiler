//! Replacements happen after capture, immediately before the real sandbox exec.
#![allow(clippy::expect_used)]
use super::{
    LaunchBytes, PluginProcessConfig, PluginSandboxProfile, SpawnedPlugin, attach_supervisor,
    helper_executable, process_fixture_lease, spawn_pinned_plugin,
};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt as _;

fn profile() -> PluginSandboxProfile {
    PluginSandboxProfile {
        mode: rw_ext::PluginSandboxMode::Approved,
        capabilities: rw_plugin_protocol::PluginCapabilities::default(),
        approved_roots: Vec::new(),
        allowed_domains: Vec::new(),
    }
}

fn fixture(script: &str) -> (tempfile::TempDir, PluginProcessConfig) {
    fixture_with_executable(std::path::Path::new("/bin/sh"), script)
}

fn native_fixture(script: &str) -> (tempfile::TempDir, PluginProcessConfig) {
    let path = std::env::var_os("PATH").expect("PATH");
    let bun = std::env::split_paths(&path)
        .map(|directory| directory.join("bun"))
        .find(|path| path.is_file())
        .expect("Bun is required for native plugin conformance");
    fixture_with_executable(&bun, script)
}

fn fixture_with_executable(
    source: &std::path::Path,
    script: &str,
) -> (tempfile::TempDir, PluginProcessConfig) {
    let directory = tempfile::tempdir().expect("code fixture");
    let root = directory.path().canonicalize().expect("canonical code");
    let executable = root.join("interpreter");
    fs::copy(source, &executable).expect("fixture executable");
    fs::write(root.join("entry.js"), script).expect("approved entry");
    fs::write(root.join("unlisted"), b"not approved").expect("unlisted file");
    let config = PluginProcessConfig::new(executable)
        .and_then(|config| config.with_cwd(&root))
        .and_then(|config| config.with_code_root(&root))
        .and_then(|config| config.with_argv(["entry.js"]))
        .and_then(|config| config.with_attested_files([root.join("entry.js")]))
        .expect("approved fixture config");
    (directory, config)
}

#[test]
fn copied_code_contains_only_attested_files_and_rejects_precapture_replacement() {
    let (_directory, config) = fixture("printf approved");
    let bytes = LaunchBytes::capture(&config, &profile()).expect("capture");
    let entry = PathBuf::from(&bytes.args(&config)[0]);
    assert_eq!(fs::read(&entry).expect("copy"), b"printf approved");
    assert!(!bytes.cwd(&config).join("unlisted").exists());
    fs::write(config.cwd().join("entry.js"), "printf replaced").expect("same-length replacement");
    assert!(LaunchBytes::capture(&config, &profile()).is_err());
    assert_eq!(fs::read(entry).expect("pinned copy"), b"printf approved");
}

#[tokio::test]
async fn postcapture_executable_and_code_replacement_cannot_change_sandbox_execution() {
    let _admission = crate::native_fixture::admit().await;
    let (_directory, config) = native_fixture(
        "import { existsSync } from 'node:fs'; if (existsSync('unlisted')) process.exit(9); process.stdout.write('approved');",
    );
    let profile = profile();
    let bytes = Arc::new(LaunchBytes::capture(&config, &profile).expect("capture"));
    let pinned_root = bytes.cwd(&config).to_path_buf();
    fs::write(config.executable(), b"not an executable anymore").expect("replace executable bytes");
    fs::write(config.cwd().join("entry.js"), "printf replaced").expect("replace code bytes");
    let scratch = tempfile::tempdir().expect("scratch");
    let helper = helper_executable().expect("explicit immutable helper prerequisite");
    let SpawnedPlugin {
        child,
        proxy,
        bytes,
    } = spawn_pinned_plugin(
        &config,
        &profile,
        scratch.path(),
        &helper,
        bytes,
        &[scratch.path().to_path_buf()],
    )
    .expect("spawn exact pinned bytes");
    let mut launched = attach_supervisor(
        child,
        proxy,
        &config,
        helper,
        process_fixture_lease(),
        bytes,
    )
    .await
    .expect("owned handoff");
    let mut output = String::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        launched.stdout.read_to_string(&mut output),
    )
    .await
    .expect("execution deadline")
    .expect("output");
    let mut diagnostics = String::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        launched.stderr.read_to_string(&mut diagnostics),
    )
    .await
    .expect("bounded stderr deadline")
    .expect("fixture stderr");
    let status = launched.process.wait().await.expect("direct child status");
    assert_eq!(
        output, "approved",
        "child status={status:?}; fixture stderr={diagnostics}"
    );
    launched
        .process
        .settle_effects()
        .await
        .expect("physical settlement");
    assert!(
        pinned_root.exists(),
        "retained process owns its immutable code"
    );
    drop(launched);
    assert!(
        !pinned_root.exists(),
        "last proven owner removes the private view"
    );
}

#[tokio::test]
async fn dropped_handoff_keeps_code_until_physical_retirement() {
    let _admission = crate::native_fixture::admit().await;
    let (_directory, config) = native_fixture(
        "process.stdout.write('ready'); await new Promise(() => { setInterval(() => {}, 1000); });",
    );
    let profile = profile();
    let bytes = Arc::new(LaunchBytes::capture(&config, &profile).expect("capture"));
    let pinned_root = bytes.cwd(&config).to_path_buf();
    let scratch = tempfile::tempdir().expect("scratch");
    let helper = helper_executable().expect("explicit immutable helper prerequisite");
    let SpawnedPlugin {
        child,
        proxy,
        bytes,
    } = spawn_pinned_plugin(
        &config,
        &profile,
        scratch.path(),
        &helper,
        bytes,
        &[scratch.path().to_path_buf()],
    )
    .expect("spawn");
    let mut launched = attach_supervisor(
        child,
        proxy,
        &config,
        helper,
        process_fixture_lease(),
        bytes,
    )
    .await
    .expect("handoff");
    assert_ready(&mut launched.stdout).await;
    assert!(pinned_root.exists());
    drop(launched);
    tokio::time::timeout(Duration::from_secs(3), async {
        while pinned_root.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("code view retires after actual child settlement");
}

#[test]
fn writable_scratch_cannot_include_or_replace_the_approved_code_view() {
    let (_directory, config) = fixture("printf approved");
    let bytes = LaunchBytes::capture(&config, &profile()).expect("capture");
    let cwd = bytes.cwd(&config).to_path_buf();
    assert!(bytes.validate_write_roots(&[cwd]).is_err());
    assert!(bytes.validate_write_roots(&[std::env::temp_dir()]).is_err());
    let unrelated = tempfile::tempdir().expect("unrelated scratch");
    bytes
        .validate_write_roots(&[unrelated.path().to_path_buf()])
        .expect("disjoint write authority");
}

#[tokio::test]
async fn unpolled_handoff_retains_then_retires_the_complete_physical_owner() {
    let _admission = crate::native_fixture::admit().await;
    let (_directory, config) = native_fixture(
        "process.stdout.write('ready'); await new Promise(() => { setInterval(() => {}, 1000); });",
    );
    let profile = profile();
    let bytes = Arc::new(LaunchBytes::capture(&config, &profile).expect("capture"));
    let pinned_root = bytes.cwd(&config).to_path_buf();
    let scratch = tempfile::tempdir().expect("scratch");
    let helper = helper_executable().expect("explicit immutable helper prerequisite");
    let SpawnedPlugin {
        child,
        proxy,
        bytes,
    } = spawn_pinned_plugin(
        &config,
        &profile,
        scratch.path(),
        &helper,
        bytes,
        &[scratch.path().to_path_buf()],
    )
    .expect("spawn");
    let mut child = child;
    assert_ready(child.stdout.as_mut().expect("child stdout")).await;
    let pending = attach_supervisor(
        child,
        proxy,
        &config,
        helper,
        process_fixture_lease(),
        bytes,
    );
    assert!(
        pinned_root.exists(),
        "unpolled handoff owns the captured bytes"
    );
    drop(pending);
    tokio::time::timeout(Duration::from_secs(3), async {
        while pinned_root.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("unpolled handoff transfers actual retirement, without leaked capacity");
}

async fn assert_ready(stdout: &mut (impl tokio::io::AsyncRead + Unpin)) {
    let mut marker = [0; 5];
    tokio::time::timeout(Duration::from_secs(3), stdout.read_exact(&mut marker))
        .await
        .expect("native fixture readiness deadline")
        .expect("native fixture must execute before retirement");
    assert_eq!(&marker, b"ready");
}

#[cfg(target_os = "macos")]
#[test]
fn preparation_read_view_is_distinct_from_immutable_executable_authority() {
    let (_directory, config) = fixture("preparation input");
    let mut policy = profile();
    policy.mode = rw_ext::PluginSandboxMode::Preparation {};
    let bytes = LaunchBytes::capture(&config, &policy).expect("capture preparation executable");
    bytes
        .validate_write_roots(&[config.cwd().to_path_buf()])
        .expect("preparation source view is governed by its existing output policy");
    assert!(
        bytes
            .validate_write_roots(&[bytes.program(&config).to_path_buf()])
            .is_err()
    );
}
