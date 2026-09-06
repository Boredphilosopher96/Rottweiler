#![allow(clippy::expect_used)]

#[cfg(target_os = "linux")]
mod linux {
    use rw_runtime::{
        executable_config::{DiscoveredPlugin, DiscoveredPluginTarget, ExecutableConfigOrigin},
        plugin::resolve_plugin_process,
    };
    use std::{fs, path::PathBuf, process::Command};
    pub(super) fn run() {
        if rw_tools::maybe_run_sandbox_helper(std::env::args_os()).expect("sandbox helper dispatch")
        {
            unreachable!("sandbox helper replaces the process")
        }
        let capability = rw_tools::probe_sandbox();
        if capability.support != rw_tools::SandboxSupport::Enforced {
            assert!(
                std::env::var_os("ROTTWEILER_REQUIRE_LINUX_SANDBOX").is_none(),
                "required Linux sandbox unavailable: {capability:?}"
            );
            eprintln!("skipping native source preparation: {capability:?}");
            return;
        }
        if let Some(root) = std::env::var_os("RW_SOURCE_PREPARATION_FIXTURE") {
            exercise(&PathBuf::from(root));
            return;
        }
        let root = tempfile::tempdir().expect("fixture");
        let package = root.path().join("package");
        fs::create_dir(&package).expect("package");
        for (name, contents) in [
            (
                "package.json",
                r#"{"dependencies":{"@rottweiler/plugin":"0.1.0"}}"#,
            ),
            ("bun.lock", r#"{"packages":{}}"#),
            ("manifest.json", r#"{"name":"preparation-fixture"}"#),
            (
                "index.ts",
                "import manifest from './manifest.json'; export default () => manifest;\n",
            ),
        ] {
            fs::write(package.join(name), contents).expect("source input");
        }
        let helper = root.path().join("source-preparation-driver");
        fs::copy(
            std::env::current_exe().expect("current executable"),
            &helper,
        )
        .expect("owned helper");
        let host = root.path().join("rottweiler-plugin-host");
        if let Some(compiled) = std::env::var_os("ROTTWEILER_PREPARATION_TEST_HOST") {
            fs::copy(compiled, &host).expect("compiled fixture host");
        } else {
            let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../packages/plugin-host/src/index.ts");
            let status = Command::new("bun")
                .args(["build", "--compile"])
                .arg(entry)
                .arg("--outfile")
                .arg(&host)
                .status()
                .expect("current host compiler");
            assert!(
                status.success(),
                "current source host compilation failed: {status}"
            );
        }
        let status = Command::new(&helper)
            .env("RW_SOURCE_PREPARATION_FIXTURE", root.path())
            .status()
            .expect("owned fixture driver");
        assert!(
            status.success(),
            "production source fixture failed: {status}"
        );
    }
    fn exercise(root: &std::path::Path) {
        let package = root.join("package");
        let helper = rw_tools::SandboxHelper::from_running(
            &std::env::current_exe().expect("current helper"),
        )
        .expect("running helper");
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
            origin: ExecutableConfigOrigin::User(root.join("plugins.toml")),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let config = resolve_plugin_process(&plugin, &root.join("private"), &helper)
                .await
                .expect("production source preparation");
            let identity = config.source_identity().expect("sealed source identity");
            assert_eq!(config.argv()[0], "run");
            let bundle = fs::read(&config.argv()[1]).expect("sealed bundle");
            assert_eq!(
                identity.bundle_blake3,
                blake3::hash(&bundle).to_hex().as_str()
            );
            assert_eq!(config.attested_files().len(), 2);
            config
                .validate_executable_identity()
                .expect("final identity");
            println!(
                "native Linux production source resolver sealed {} bytes",
                bundle.len()
            );
        });
    }
}
#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}
#[cfg(not(target_os = "linux"))]
fn main() {}
