use rw_ext::{ApprovalStore, ApprovalStoreError, DenyPushHandler, PluginHost, PluginProcessConfig};
use rw_runtime::plugin::SandboxedPluginLauncher;
use std::{
    collections::BTreeMap,
    io::Read as _,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Approvals(Mutex<BTreeMap<String, String>>);
impl ApprovalStore for Approvals {
    fn approved_fingerprint(&self, name: &str) -> Result<Option<String>, ApprovalStoreError> {
        Ok(self.0.lock().expect("approval lock").get(name).cloned())
    }
    fn record_approval(&self, name: &str, fingerprint: &str) -> Result<(), ApprovalStoreError> {
        self.0
            .lock()
            .expect("approval lock")
            .insert(name.into(), fingerprint.into());
        Ok(())
    }
}
struct Redactor;
impl rw_ext::PluginBoundaryRedactor for Redactor {
    fn redact_reply_text(
        &self,
        text: &str,
        max_bytes: usize,
    ) -> Result<String, rw_ext::PluginRpcError> {
        if text.len() > max_bytes {
            return Err(rw_ext::PluginRpcError {
                code: "reply_admission".into(),
                message: "fixture reply exceeds admission".into(),
            });
        }
        Ok(text.to_owned())
    }
    fn redact(&self, value: serde_json::Value) -> serde_json::Value {
        value
    }
}
fn input(name: &str, limit: u64) -> Vec<u8> {
    let path = PathBuf::from(std::env::var_os(name).expect("explicit acceptance artifact input"));
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .expect("artifact input")
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .expect("bounded input");
    assert!(bytes.len() as u64 <= limit, "artifact input limit");
    bytes
}
pub struct Fixture {
    root: tempfile::TempDir,
    launcher: SandboxedPluginLauncher,
    config: PluginProcessConfig,
    manifest: rw_plugin_protocol::PluginManifest,
    _sdk: rw_tools::ApprovedExecutable,
}
impl Fixture {
    pub fn load() -> Self {
        let native = serde_json::from_slice(&input("ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT", 4096))
            .expect("native artifact identity");
        let helper =
            rw_tools::SandboxHelper::from_artifact(&native).expect("verified native helper bytes");
        let sdk: rw_tools::ExecutableArtifactIdentity =
            serde_json::from_slice(&input("ROTTWEILER_LONG_TOOL_RECEIPT", 4096))
                .expect("compiled SDK identity");
        let root = tempfile::tempdir().expect("acceptance scratch");
        let config = PluginProcessConfig::new(&sdk.executable)
            .expect("compiled SDK process config")
            .with_cwd(root.path())
            .expect("SDK working directory")
            .with_code_root(sdk.executable.parent().expect("SDK package"))
            .expect("owned SDK code package");
        let approved =
            rw_tools::ApprovedExecutable::from_artifact(&sdk).expect("verified compiled SDK bytes");
        let identity = config.executable_identity();
        assert_eq!(
            (identity.device, identity.inode, identity.length),
            (sdk.device, sdk.inode, sdk.bytes)
        );
        config
            .validate_executable_identity()
            .expect("unchanged SDK identity");
        let launcher =
            SandboxedPluginLauncher::new(root.path(), &helper).expect("enforced native launcher");
        let manifest = rw_plugin_protocol::PluginManifest::from_slice(&input(
            "ROTTWEILER_LONG_TOOL_MANIFEST",
            256 * 1024,
        ))
        .expect("source fixture manifest");
        Self {
            root,
            launcher,
            config,
            manifest,
            _sdk: approved,
        }
    }
    pub async fn launch(&self) -> Arc<PluginHost> {
        let approvals = Approvals::default();
        rw_ext::approve_plugin_launch(
            &approvals,
            &self.manifest,
            &self.config,
            "native-long-tool-acceptance",
        )
        .expect("exact artifact approval");
        Arc::new(
            PluginHost::launch_approved(
                &self.launcher,
                Arc::new(approvals),
                &self.config,
                "native-long-tool-acceptance",
                &[self.root.path().to_path_buf()],
                self.manifest.clone(),
                Arc::new(DenyPushHandler),
                Arc::new(Redactor),
            )
            .await
            .expect("native code-only SDK launch"),
        )
    }
}
