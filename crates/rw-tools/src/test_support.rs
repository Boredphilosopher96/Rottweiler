#![allow(clippy::expect_used)]
pub(crate) fn sandbox_helper() -> rw_sandbox::SandboxHelper {
    use std::io::Read as _;
    static IDENTITY: std::sync::OnceLock<rw_sandbox::ExecutableArtifactIdentity> =
        std::sync::OnceLock::new();
    let identity = IDENTITY.get_or_init(|| {
            let receipt = std::env::var_os("ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT")
                .expect("native fixture prerequisite: run scripts/build-test-helper.py and export ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT");
            let mut bytes = Vec::new();
            std::fs::File::open(receipt)
                .expect("open sandbox helper receipt")
                .take(4097)
                .read_to_end(&mut bytes)
                .expect("read sandbox helper receipt");
            assert!(bytes.len() <= 4096, "sandbox helper receipt exceeds 4096 bytes");
            serde_json::from_slice(&bytes).expect("strict sandbox helper identity")
        });
    rw_sandbox::SandboxHelper::from_artifact(identity).expect("verify sandbox helper artifact")
}
