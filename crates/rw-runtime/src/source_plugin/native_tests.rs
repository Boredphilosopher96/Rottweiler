#![allow(clippy::expect_used)]
use super::*;
use crate::{extension_config::ExecutableConfigOrigin, plugin_process::SandboxedPluginLauncher};
use async_trait::async_trait;
use rw_ext::{
    LaunchedPluginProcess, PluginLaunchError, PluginSandboxProfile, SupervisedPluginProcess,
};
use std::{process::Stdio, sync::Mutex};

struct RecordingLauncher {
    inner: SandboxedPluginLauncher,
    processes: Mutex<Vec<Arc<dyn SupervisedPluginProcess>>>,
}
#[async_trait]
impl PluginLauncher for RecordingLauncher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> std::result::Result<LaunchedPluginProcess, PluginLaunchError> {
        let child = self.inner.launch(config, profile).await?;
        self.processes
            .lock()
            .expect("processes")
            .push(Arc::clone(&child.process));
        Ok(child)
    }
}

#[tokio::test]
async fn source_resolver_seals_current_host_output_after_native_helpers_settle() {
    let _admission = crate::native_fixture::admit().await;
    let root = tempfile::tempdir().expect("fixture root");
    let package = root.path().join("package");
    fs::create_dir(&package).expect("package directory");
    let package = fs::canonicalize(package).expect("canonical package directory");
    let host = root.path().join("rottweiler-plugin-host");
    compile_source_host(root.path(), &host).await;
    fs::write(
        package.join("package.json"),
        r#"{"dependencies":{"@rottweiler/plugin":"0.1.0"}}"#,
    )
    .expect("package metadata");
    fs::write(package.join("bun.lock"), r#"{"packages":{}}"#).expect("lock metadata");
    fs::write(
        package.join("manifest.json"),
        r#"{"name":"preparation-fixture"}"#,
    )
    .expect("manifest");
    fs::write(
        package.join("index.ts"),
        "import manifest from './manifest.json'; export default () => manifest;\n",
    )
    .expect("source entry");
    let scratch = Arc::new(crate::extension_runtime::PrivateMcpScratch::create().expect("scratch"));
    let scratch_path = scratch.path().to_owned();
    let launcher = Arc::new(RecordingLauncher {
        inner: SandboxedPluginLauncher::new(
            scratch.path(),
            &std::env::current_exe().expect("helper"),
        )
        .expect("native sandbox launcher"),
        processes: Mutex::new(Vec::new()),
    });
    let resolver = SourcePluginResolver::new(
        &host,
        &root.path().join("private"),
        scratch,
        launcher.clone(),
        Arc::new(SourcePreparationBudget::default()),
    )
    .expect("source resolver");
    let plugin = DiscoveredPlugin {
        name: "preparation-fixture".to_owned(),
        enabled: true,
        target: DiscoveredPluginTarget::TypeScript {
            package_root: package.clone(),
            entry: package.join("index.ts"),
        },
        inherit_env: Vec::new(),
        manifest_path: package.join("manifest.json"),
        allowed_domains: Vec::new(),
        origin: ExecutableConfigOrigin::User(root.path().join("plugins.toml")),
    };
    let config = resolver
        .resolve(&plugin)
        .await
        .expect("sealed source config");
    let identity = config.source_identity().expect("source identity");
    assert_eq!(config.argv()[0], "run");
    let bundle = fs::read(&config.argv()[1]).expect("sealed bundle");
    assert_eq!(
        identity.bundle_blake3,
        blake3::hash(&bundle).to_hex().as_str()
    );
    assert_eq!(config.attested_files().len(), 2);
    config
        .validate_executable_identity()
        .expect("attested config validates");
    let processes = launcher.processes.lock().expect("processes").clone();
    assert_eq!(processes.len(), 2, "one graph and one bundle helper");
    for process in processes {
        assert_eq!(
            process.wait().await.expect("helper already reaped"),
            Some(0)
        );
        process
            .settle_effects()
            .await
            .expect("whole helper group settled");
    }
    drop(resolver);
    assert!(!scratch_path.exists());
    assert!(
        Path::new(&config.argv()[1]).exists(),
        "sealed output outlives scratch"
    );
}

#[tokio::test]
async fn preparation_directory_grants_do_not_read_siblings_or_recur() {
    let _admission = crate::native_fixture::admit().await;
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fs::canonicalize(fixture.path()).expect("root");
    let package = root.join("package");
    fs::create_dir(&package).expect("package");
    fs::create_dir(root.join("sibling")).expect("sibling");
    fs::write(root.join("secret"), "private content\n").expect("secret");
    fs::write(root.join("sibling/hidden"), "private content\n").expect("nested secret");
    let scratch = Arc::new(crate::extension_runtime::PrivateMcpScratch::create().expect("scratch"));
    let launcher: Arc<dyn PluginLauncher> = Arc::new(
        SandboxedPluginLauncher::new(scratch.path(), &std::env::current_exe().expect("helper"))
            .expect("sandbox launcher"),
    );
    let config = PluginProcessConfig::new("/bin/sh")
        .and_then(|config| config.with_cwd(&package))
        .and_then(|config| config.with_code_root(&package))
        .and_then(|config| {
            config.with_argv([
                "-c".to_owned(),
                r#"set -- "$1" "$1"/*
case "$*" in *secret*) ;; *) exit 11 ;; esac
if IFS= read -r content < "$1/secret"; then exit 12; fi
for entry in "$1/sibling"/*; do
    case "$entry" in */hidden) exit 13 ;; esac
done
printf 'only exact ancestor entries\n'
"#
                .to_owned(),
                "preparation".to_owned(),
                root.to_string_lossy().into_owned(),
            ])
        })
        .expect("preparation config");
    let pool = Arc::new(SourcePreparations::default());
    let output = pool
        .execute(
            PreparationRequest {
                config,
                output_root: None,
                launcher,
                scratch,
            },
            tokio::time::Instant::now() + HOST_DEADLINE,
        )
        .await
        .expect("preparation settles");
    assert_eq!(
        output.status,
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"only exact ancestor entries\n");
}

async fn compile_source_host(root: &Path, host: &Path) {
    let temporary = tempfile::Builder::new()
        .prefix("compiler-")
        .tempdir_in(root)
        .expect("compiler temporary directory");
    let entry =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/plugin-host/src/index.ts");
    let mut compiler = tokio::process::Command::new("bun")
        .args(["build", "--compile"])
        .arg(entry)
        .arg("--outfile")
        .arg(host)
        .current_dir(root)
        .env("TMPDIR", temporary.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("compile current source host");
    let status =
        if let Ok(status) = tokio::time::timeout(Duration::from_secs(30), compiler.wait()).await {
            status.expect("compiler status")
        } else {
            let _ = compiler.start_kill();
            compiler.wait().await.expect("reap compiler");
            panic!("source host compilation exceeded 30 seconds");
        };
    assert!(status.success(), "source host compilation failed");
}
