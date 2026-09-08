#![allow(clippy::expect_used)]
use super::PluginRuntimeBudget;
use std::sync::Arc;

#[tokio::test]
async fn application_close_requires_physical_executable_retirement() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().expect("fixture");
    let path = directory.path().join("approved");
    std::fs::write(&path, b"immutable").expect("source");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("mode");
    let receipt = rw_tools::ExecutableArtifactIdentity::capture(
        &path.canonicalize().expect("canonical"),
        1024,
    )
    .expect("receipt");
    let budget = Arc::new(PluginRuntimeBudget::default());
    let child_budget = Arc::clone(&budget);
    let first = budget.images.acquire(&receipt).expect("first generation");
    let second = child_budget
        .images
        .acquire(&receipt)
        .expect("second generation");
    let launch = first.launch().expect("physical process image");
    drop((first, second, child_budget));
    assert_eq!(
        budget.close().await.expect_err("live image").code,
        "effects_unsettled"
    );
    assert!(budget.images.acquire(&receipt).is_err());
    assert_eq!(
        std::fs::read(launch.path()).expect("retained after close"),
        b"immutable"
    );
    drop(launch);
    assert!(
        budget.close().await.is_err(),
        "failed application proof remains explicit"
    );
    budget.images.close().expect("actual image retirement");
}

#[tokio::test]
async fn unused_application_image_owner_closes_without_filesystem_work() {
    use futures_util::FutureExt as _;
    let budget = PluginRuntimeBudget::default();
    budget
        .close()
        .now_or_never()
        .expect("unused owner dispatches no worker")
        .expect("inert owner");
    budget.close().await.expect("idempotent proof");
}
