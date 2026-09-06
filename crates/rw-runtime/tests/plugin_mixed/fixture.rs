use super::{process, wasm};
use rw_runtime::executable_config::discover_executable_configs;
use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    process::Command,
};

pub struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    storage: PathBuf,
    workspace: PathBuf,
    engine: PathBuf,
    native_manifest: PathBuf,
    _native: rw_tools::ApprovedExecutable,
    _helper: rw_tools::SandboxHelper,
}
impl Fixture {
    pub async fn prepare() -> Self {
        let engine: rw_tools::ExecutableArtifactIdentity = input("ROTTWEILER_MIXED_ENGINE_RECEIPT");
        let helper =
            rw_tools::SandboxHelper::from_artifact(&engine).expect("approved native bundle");
        let native: rw_tools::ExecutableArtifactIdentity = input("ROTTWEILER_MIXED_NATIVE_RECEIPT");
        let approved_native =
            rw_tools::ApprovedExecutable::from_artifact(&native).expect("approved compiled SDK");
        let manifest: rw_plugin_protocol::PluginManifest =
            input("ROTTWEILER_MIXED_NATIVE_MANIFEST");
        let root = tempfile::tempdir().expect("fixture root");
        let base = root.path().canonicalize().expect("fixture identity");
        let home = directory(&base.join("home"));
        let storage = directory(&home.join(".rottweiler"));
        rw_runtime::session::initialize_private_storage_root(&storage).expect("private storage");
        fs::write(
            storage.join("config.toml"),
            r#"[models]
default = "fixture"
[models.aliases]
fixture = ["fixture/base"]
[providers.fixture]
kind = "openai_compatible"
base_url = "http://127.0.0.1:1/v1/chat/completions"
"#,
        )
        .expect("inert provider configuration");
        let workspace = directory(&base.join("workspace"));
        fs::write(workspace.join("input.txt"), "WASM must block this read").expect("tool input");
        let package = directory(&workspace.join("native"));
        let native_manifest = package.join("manifest.json");
        write_json(&native_manifest, &manifest);
        let hanging_manifest = package.join("hanging.json");
        write_json(
            &hanging_manifest,
            &serde_json::json!({
                "name":"hanging-native","version":"1.0.0","protocol":3,
                "capabilities":{"commands":[{"name":"never-ready","description":"Never initializes","allowed_tools":[]}]}
            }),
        );
        let source = source_package(&workspace);
        let settings = storage.join("plugins.toml");
        fs::write(&settings, toml::to_string(&serde_json::json!({"plugins":[
            {"name":"mixed-native","argv":[native.executable],"manifest":native_manifest,"cwd":workspace},
            {"name":"hanging-native","argv":[native.executable,"--hang"],"manifest":hanging_manifest,"cwd":workspace},
            {"name":"mixed-source","source":source}
        ]})).expect("settings encoding")).expect("settings");
        let catalog = discover_executable_configs(&home, &workspace, false).expect("mixed catalog");
        assert_eq!(catalog.plugins.len(), 3);
        let approvals =
            rw_runtime::PrivatePluginApprovalStore::open(&storage).expect("approval owner");
        for plugin in &catalog.plugins {
            let process = rw_runtime::plugin::resolve_plugin_process(plugin, &storage, &helper)
                .await
                .expect("sealed process identity");
            rw_ext::approve_plugin_launch(
                &approvals,
                &plugin.load_manifest().expect("manifest"),
                &process,
                &format!("user:{}", settings.display()),
            )
            .expect("exact artifact approval");
        }
        wasm::install(
            &storage.join("extensions"),
            &bytes("ROTTWEILER_MIXED_WASM_COMPONENT", 1024 * 1024),
        );
        Self {
            _root: root,
            home,
            storage,
            workspace,
            engine: engine.executable,
            native_manifest,
            _native: approved_native,
            _helper: helper,
        }
    }
    pub async fn run(&self, command: &str) -> process::Output {
        let mut process = Command::new(&self.engine);
        process
            .args(["-p", command, "--output-format", "json"])
            .env_clear()
            .env("HOME", &self.home)
            .env("ROTTWEILER_HOME", &self.storage)
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&self.workspace);
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            process::run(process)
        })
        .await
        .expect("owned candidate process")
    }
    pub fn change_capabilities(&self) {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&self.native_manifest).expect("manifest bytes"))
                .expect("manifest");
        manifest["capabilities"]["push"]
            .as_array_mut()
            .expect("push declarations")
            .push(serde_json::json!("session/query"));
        write_json(&self.native_manifest, &manifest);
    }
}
fn directory(path: &Path) -> PathBuf {
    fs::create_dir(path).expect("fixture directory");
    path.to_path_buf()
}
fn write_json(path: &Path, value: &impl serde::Serialize) {
    fs::write(path, serde_json::to_vec(value).expect("JSON encoding")).expect("JSON file");
}
fn bytes(name: &str, limit: u64) -> Vec<u8> {
    let path = std::env::var_os(name).expect("explicit mixed acceptance artifact");
    let mut bytes = Vec::new();
    fs::File::open(path)
        .expect("artifact input")
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .expect("bounded input");
    assert!(bytes.len() as u64 <= limit);
    bytes
}
fn input<T: serde::de::DeserializeOwned>(name: &str) -> T {
    serde_json::from_slice(&bytes(name, 256 * 1024)).expect("typed fixture input")
}
fn source_package(workspace: &Path) -> PathBuf {
    let package = directory(&workspace.join("source"));
    directory(&package.join("src"));
    write_json(
        &package.join("package.json"),
        &serde_json::json!({"name":"mixed-source","version":"1.0.0","type":"module","dependencies":{"@rottweiler/plugin":env!("CARGO_PKG_VERSION")}}),
    );
    fs::write(package.join("bun.lock"), r#"{"packages":{}}"#).expect("empty dependency lock");
    write_json(
        &package.join("manifest.json"),
        &serde_json::json!({
            "name":"mixed-source","version":"1.0.0","protocol":3,
            "capabilities":{"commands":[{"name":"source-ready","description":"Sealed source command","allowed_tools":[]}]}
        }),
    );
    fs::write(package.join("src/index.ts"), "import manifest from '../manifest.json'; export default {manifest,handlers:{commands:{'source-ready':()=>({result:'SOURCE_READY'})}}};\n").expect("source entry");
    package
}
