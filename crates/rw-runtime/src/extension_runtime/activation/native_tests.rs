#![allow(clippy::expect_used)]
use super::*;
use crate::extension_runtime::tests::rollback_plugin;
use crate::extension_runtime::{
    PrivateMcpScratch, PrivatePluginApprovalStore, SessionPluginPushHandler, SharedPluginRedactor,
};
use rw_ext::{
    LaunchedPluginProcess, PluginLaunchError, PluginLauncher, PluginProcessConfig,
    PluginSandboxProfile, SupervisedPluginProcess,
};
use std::os::unix::fs::PermissionsExt as _;
use tokio::{io::AsyncBufReadExt as _, sync::Semaphore};

struct HeldNativeLaunch {
    inner: crate::plugin_process::SandboxedPluginLauncher,
    _scratch: Arc<PrivateMcpScratch>,
    process: Mutex<Option<Arc<dyn SupervisedPluginProcess>>>,
    admitted: Notify,
    release: Semaphore,
}
#[async_trait]
impl PluginLauncher for HeldNativeLaunch {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginLaunchError> {
        let mut launched = self.inner.launch(config, profile).await?;
        *self.process.lock().expect("native process owner") = Some(Arc::clone(&launched.process));
        let mut ready = String::new();
        launched
            .stderr
            .read_line(&mut ready)
            .await
            .expect("native startup marker");
        assert_eq!(ready.trim(), "native-ready");
        self.admitted.notify_one();
        self.release
            .acquire()
            .await
            .expect("release native handoff")
            .forget();
        Ok(launched)
    }
}

#[tokio::test]
async fn aborted_first_use_waits_for_real_sandboxed_process_handoff_and_reap() {
    let _admission = crate::native_fixture::admit().await;
    let root = tempfile::tempdir().expect("fixture root");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private fixture root");
    let (mut config, manifest) = rollback_plugin(root.path(), "native_lazy");
    let package = config.manifest_path.parent().expect("package");
    let compiled = compile_worker(package, &manifest).await;
    config.target = crate::extension_config::DiscoveredPluginTarget::Executable {
        argv: vec![compiled.to_string_lossy().into_owned()],
        cwd: package.to_path_buf(),
    };
    let store = PrivatePluginApprovalStore::open(root.path()).expect("approval store");
    rw_ext::approve_plugin_launch(
        &store,
        &manifest,
        &config.executable_process_config().expect("process config"),
        &format!("user:{}", config.origin.path().display()),
    )
    .expect("approve exact native fixture");
    let scratch = Arc::new(PrivateMcpScratch::create().expect("native scratch"));
    let launcher = Arc::new(HeldNativeLaunch {
        inner: crate::plugin_process::SandboxedPluginLauncher::new(
            scratch.path(),
            &crate::plugin_process::helper_executable()
                .expect("fixture sandbox helper prerequisite"),
        )
        .expect("native sandbox launcher"),
        _scratch: scratch,
        process: Mutex::new(None),
        admitted: Notify::new(),
        release: Semaphore::new(0),
    });
    let budget = Arc::new(PluginRuntimeBudget::default());
    let endpoint = Arc::new(DormantPluginEndpoint::new(ActivationRecipe {
        approval: ActivationApproval::Configured,
        metadata: PluginEndpointMetadata::new(manifest).expect("metadata"),
        config,
        private_root: root.path().to_path_buf(),
        workspace_roots: vec![root.path().to_path_buf()],
        helper: crate::extension_runtime::SandboxHelperSource::pending(),
        redactor: Arc::new(SharedPluginRedactor::new(
            rw_providers::FixtureRedactor::default(),
        )),
        push_handler: Arc::new(SessionPluginPushHandler::default()),
        budget: Arc::clone(&budget),
        launcher: Some(launcher.clone()),
    }));
    let connection = Arc::clone(&endpoint);
    let mut waiter =
        tokio::spawn(async move { connection.connect(&CancellationToken::default()).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            () = launcher.admitted.notified() => {},
            result = &mut waiter => panic!("first use ended before native handoff: {:?}", result.expect("waiter task").err()),
        }
    }).await.expect("real native launch admitted");
    let process = launcher
        .process
        .lock()
        .expect("native process")
        .clone()
        .expect("actual child owner");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), process.wait())
            .await
            .is_err(),
        "native child is alive before cancellation"
    );
    waiter.abort();
    assert!(waiter.await.err().expect("waiter cancelled").is_cancelled());
    let connection = Arc::clone(&endpoint);
    let barrier = tokio::spawn(async move { connection.settle_effects().await });
    tokio::task::yield_now().await;
    assert!(!barrier.is_finished());
    launcher.release.add_permits(1);
    barrier
        .await
        .expect("barrier task")
        .expect("native handoff and cleanup proved");
    assert_eq!(process.wait().await.expect("actual child reaped"), Some(0));
    process
        .settle_effects()
        .await
        .expect("actual process group and proxy settled");
    budget.close().expect("native activation capacity returned");
}

async fn compile_worker(
    package: &std::path::Path,
    manifest: &rw_plugin_protocol::PluginManifest,
) -> std::path::PathBuf {
    let script = package.join("worker.c");
    let manifest_json = serde_json::to_string(&manifest).expect("manifest JSON");
    std::fs::write(
        &script,
        r#"
#include <stdio.h>
#include <string.h>
int main(void) {
  char line[16384];
  fputs("native-ready\n", stderr); fflush(stderr);
  while (fgets(line, sizeof(line), stdin)) {
    if (strstr(line, "\"method\":\"exit\"")) return 0;
    char *id = strstr(line, "\"id\":");
    unsigned long long request;
    if (!id || sscanf(id + 5, "%llu", &request) != 1) return 2;
    const char *result = strstr(line, "\"method\":\"initialize\"") ? __MANIFEST__ : "null";
    printf("{\"jsonrpc\":\"2.0\",\"id\":%llu,\"result\":%s}\n", request, result);
    fflush(stdout);
  }
  return 0;
}
"#
        .replace(
            "__MANIFEST__",
            &serde_json::to_string(&manifest_json).expect("C string"),
        ),
    )
    .expect("native protocol fixture");
    let executable = package.join("native-worker");
    let temporary = tempfile::Builder::new()
        .prefix("compiler-")
        .tempdir_in(package)
        .expect("compiler scratch");
    let mut compiler = tokio::process::Command::new("/usr/bin/cc")
        .args(["-Wall", "-Wextra", "-Werror"])
        .arg(&script)
        .arg("-o")
        .arg(&executable)
        .env("TMPDIR", temporary.path())
        .current_dir(package)
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("compile native fixture");
    let status =
        if let Ok(status) = tokio::time::timeout(Duration::from_secs(30), compiler.wait()).await {
            status.expect("compiler status")
        } else {
            let _ = compiler.start_kill();
            compiler.wait().await.expect("compiler reaped");
            panic!("native fixture compilation exceeded 30 seconds");
        };
    assert!(status.success());
    executable
}
