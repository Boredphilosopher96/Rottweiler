#![allow(clippy::expect_used)]
use super::*;
use crate::extension_runtime::tests::{FailSecondPluginLauncher, RollbackProcess, rollback_plugin};
use crate::extension_runtime::{
    PrivatePluginApprovalStore, SessionPluginPushHandler, SharedPluginRedactor,
};
use rw_ext::{
    LaunchedPluginProcess, PluginLaunchError, PluginLauncher, PluginProcessConfig,
    PluginSandboxProfile,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

struct DelayedLauncher {
    inner: Arc<FailSecondPluginLauncher>,
    admitted: Notify,
    release: Semaphore,
    returned: AtomicUsize,
}
#[async_trait]
impl PluginLauncher for DelayedLauncher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginLaunchError> {
        self.admitted.notify_one();
        self.release
            .acquire()
            .await
            .expect("launch release")
            .forget();
        let result = self.inner.launch(config, profile).await;
        self.returned.fetch_add(1, Ordering::AcqRel);
        result
    }
}

struct Fixture {
    root: tempfile::TempDir,
    endpoint: Arc<DormantPluginEndpoint>,
    launcher: Arc<DelayedLauncher>,
    process: Arc<RollbackProcess>,
    budget: Arc<PluginActivationBudget>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_budget(Arc::new(PluginActivationBudget::default()))
    }
    fn with_budget(budget: Arc<PluginActivationBudget>) -> Self {
        Self::with_approval(budget, ActivationApproval::Configured)
    }
    fn with_approval(budget: Arc<PluginActivationBudget>, approval: ActivationApproval) -> Self {
        let root = tempfile::tempdir().expect("root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
                .expect("private root");
        }
        let (config, manifest) = rollback_plugin(root.path(), "lazy");
        if matches!(approval, ActivationApproval::Configured) {
            let store = PrivatePluginApprovalStore::open(root.path()).expect("approval store");
            rw_ext::approve_plugin_launch(
                &store,
                &manifest,
                &config.executable_process_config().expect("process"),
                &format!("user:{}", config.origin.path().display()),
            )
            .expect("approval");
        }
        let process = Arc::new(RollbackProcess::default());
        let launcher = Arc::new(DelayedLauncher {
            inner: Arc::new(FailSecondPluginLauncher {
                launches: AtomicUsize::new(0),
                first_manifest: manifest.clone(),
                first_process: Arc::clone(&process),
            }),
            admitted: Notify::new(),
            release: Semaphore::new(0),
            returned: AtomicUsize::new(0),
        });
        let endpoint = Arc::new(DormantPluginEndpoint::new(ActivationRecipe {
            approval,
            metadata: PluginEndpointMetadata::new(manifest).expect("metadata"),
            config,
            private_root: root.path().to_path_buf(),
            workspace_roots: vec![root.path().to_path_buf()],
            helper: std::env::current_exe().expect("helper"),
            redactor: Arc::new(SharedPluginRedactor::new(
                rw_providers::FixtureRedactor::default(),
            )),
            push_handler: Arc::new(SessionPluginPushHandler::default()),
            budget: Arc::clone(&budget),
            launcher: Some(launcher.clone()),
        }));
        Self {
            root,
            endpoint,
            launcher,
            process,
            budget,
        }
    }
    fn connect(&self) -> tokio::task::JoinHandle<Result<PluginConnection, PluginRpcError>> {
        let endpoint = Arc::clone(&self.endpoint);
        tokio::spawn(async move { endpoint.connect(&CancellationToken::default()).await })
    }
}

#[tokio::test]
async fn metadata_and_closed_dormant_generation_start_no_resources() {
    let fixture = Fixture::new();
    assert!(matches!(
        fixture.endpoint.generation.snapshot(),
        Phase::Dormant
    ));
    assert!(
        fixture
            .endpoint
            .generation
            .resources
            .lock()
            .expect("resources")
            .scratch
            .is_none()
    );
    assert_eq!(fixture.launcher.inner.launches.load(Ordering::Acquire), 0);
    fixture.endpoint.close().await.expect("dormant closure");
    assert!(
        fixture
            .endpoint
            .connect(&CancellationToken::default())
            .await
            .is_err()
    );
    assert_eq!(fixture.launcher.returned.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn concurrent_first_calls_share_one_owned_launch() {
    let fixture = Fixture::new();
    let first = fixture.connect();
    fixture.launcher.admitted.notified().await;
    let second = fixture.connect();
    tokio::task::yield_now().await;
    fixture.launcher.release.add_permits(1);
    assert!(first.await.expect("first waiter").is_ok());
    assert!(second.await.expect("second waiter").is_ok());
    assert_eq!(fixture.launcher.returned.load(Ordering::Acquire), 1);
    fixture.endpoint.close().await.expect("settled close");
    assert!(fixture.process.waited.load(Ordering::Acquire) > 0);
    assert!(
        fixture
            .endpoint
            .connect(&CancellationToken::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dropped_waiter_keeps_launch_owned_then_retires_late_initialized_host() {
    let fixture = Fixture::new();
    let waiter = fixture.connect();
    fixture.launcher.admitted.notified().await;
    waiter.abort();
    assert!(waiter.await.err().expect("aborted waiter").is_cancelled());
    assert_eq!(fixture.launcher.returned.load(Ordering::Acquire), 0);
    let endpoint = Arc::clone(&fixture.endpoint);
    let barrier = tokio::spawn(async move { endpoint.settle_effects().await });
    tokio::task::yield_now().await;
    assert!(
        !barrier.is_finished(),
        "future drop cannot prove launch settlement"
    );
    fixture.launcher.release.add_permits(1);
    barrier
        .await
        .expect("barrier task")
        .expect("late host retired");
    assert_eq!(fixture.launcher.returned.load(Ordering::Acquire), 1);
    assert!(fixture.process.waited.load(Ordering::Acquire) > 0);
    assert!(
        fixture
            .endpoint
            .generation
            .resources
            .lock()
            .expect("resources")
            .scratch
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn launch_ignoring_total_deadline_retains_owner_and_sticky_failed_proof() {
    let fixture = Fixture::new();
    let waiter = fixture.connect();
    fixture.launcher.admitted.notified().await;
    tokio::time::advance(ACTIVATION_DEADLINE + PROOF_DEADLINE + Duration::from_secs(1)).await;
    let result = waiter.await.expect("waiter ended");
    assert_eq!(
        result.err().expect("failed proof").code,
        "effects_unsettled"
    );
    assert_eq!(fixture.launcher.returned.load(Ordering::Acquire), 0);
    assert!(
        fixture
            .endpoint
            .generation
            .resources
            .lock()
            .expect("resources")
            .scratch
            .is_some()
    );
    fixture.launcher.release.add_permits(1);
    loop {
        if matches!(fixture.endpoint.generation.snapshot(), Phase::Closed { .. }) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        fixture.endpoint.close().await.is_err(),
        "failed proof stays sticky"
    );
    assert!(
        fixture
            .endpoint
            .generation
            .resources
            .lock()
            .expect("resources")
            .lease
            .is_some()
    );
}

#[tokio::test]
async fn application_close_rejects_active_capacity_and_prevents_new_admission() {
    let fixture = Fixture::new();
    let waiter = fixture.connect();
    fixture.launcher.admitted.notified().await;
    assert!(fixture.budget.close().is_err());
    assert!(fixture.budget.admit().is_err());
    waiter.abort();
    let _ = waiter.await;
    fixture.launcher.release.add_permits(1);
    fixture
        .endpoint
        .close()
        .await
        .expect("owned launch retired");
}

#[tokio::test]
async fn source_and_provider_metadata_registration_performs_no_activation() {
    let fixture = Fixture::new();
    let mut config = fixture.endpoint.generation.recipe.config.clone();
    let mut manifest = fixture.endpoint.metadata().manifest().clone();
    manifest
        .capabilities
        .providers
        .push(rw_plugin_protocol::PluginProviderCapability {
            alias_prefix: "lazy/".to_owned(),
            capabilities: vec!["models".to_owned()],
            credential_references: Vec::new(),
        });
    std::fs::write(
        &config.manifest_path,
        serde_json::to_vec(&manifest).expect("manifest"),
    )
    .expect("manifest file");
    config.target = crate::extension_config::DiscoveredPluginTarget::TypeScript {
        package_root: fixture.root.path().join("unprepared-source"),
        entry: fixture.root.path().join("unprepared-source/index.ts"),
    };
    let runtime = crate::extension_runtime::PluginSessionRuntime::compose(
        &[config],
        fixture.root.path(),
        &[fixture.root.path().to_path_buf()],
        &fixture.root.path().join("missing-release/rw"),
        &fixture.endpoint.generation.recipe.redactor,
        &fixture.budget,
    )
    .expect("inert composition");
    assert_eq!(runtime.providers.len(), 1);
    assert_eq!(runtime.endpoints.len(), 1);
    assert!(runtime.pending.is_empty());
    runtime.shutdown().await.expect("never activated");
    fixture.budget.close().expect("no retained capacity");
}

#[tokio::test]
async fn cancelling_queued_generation_releases_its_slots_without_native_launch() {
    let budget = Arc::new(PluginActivationBudget::default());
    let first = Fixture::with_budget(Arc::clone(&budget));
    let second = Fixture::with_budget(Arc::clone(&budget));
    let queued = Fixture::with_budget(Arc::clone(&budget));
    let a = first.connect();
    first.launcher.admitted.notified().await;
    let b = second.connect();
    second.launcher.admitted.notified().await;
    let c = queued.connect();
    while matches!(queued.endpoint.generation.snapshot(), Phase::Dormant) {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    c.abort();
    let _ = c.await;
    queued
        .endpoint
        .settle_effects()
        .await
        .expect("queued cancellation settled");
    assert_eq!(queued.launcher.returned.load(Ordering::Acquire), 0);
    assert_eq!(queued.launcher.inner.launches.load(Ordering::Acquire), 0);
    first.launcher.release.add_permits(1);
    second.launcher.release.add_permits(1);
    assert!(a.await.expect("first task").is_ok());
    assert!(b.await.expect("second task").is_ok());
    first.endpoint.close().await.expect("first closed");
    second.endpoint.close().await.expect("second closed");
    budget
        .close()
        .expect("all queued and running capacity returned");
}

#[tokio::test]
async fn exhausted_waiter_admission_does_not_close_an_inert_generation() {
    let fixture = Fixture::new();
    let slots = (0..32)
        .map(|_| fixture.budget.waiter().expect("bounded waiter slot"))
        .collect::<Vec<_>>();
    let result = fixture
        .endpoint
        .connect(&CancellationToken::default())
        .await;
    assert_eq!(result.err().expect("busy").code, "busy");
    assert!(matches!(
        fixture.endpoint.generation.snapshot(),
        Phase::Dormant
    ));
    drop(slots);
    let waiter = fixture.connect();
    fixture.launcher.admitted.notified().await;
    fixture.launcher.release.add_permits(1);
    assert!(waiter.await.expect("waiter").is_ok());
    fixture.endpoint.close().await.expect("closed");
    fixture.budget.close().expect("capacity returned");
}

#[tokio::test]
async fn zero_ten_and_fifty_installed_plugins_remain_inert() {
    let fixture = Fixture::new();
    for count in [0, 10, 50] {
        let configs = (0..count)
            .map(|index| {
                rollback_plugin(fixture.root.path(), &format!("installed_{count}_{index}")).0
            })
            .collect::<Vec<_>>();
        let budget = Arc::new(PluginActivationBudget::default());
        let runtime = crate::extension_runtime::PluginSessionRuntime::compose(
            &configs,
            fixture.root.path(),
            &[fixture.root.path().to_path_buf()],
            &fixture.root.path().join("unavailable-helper"),
            &fixture.endpoint.generation.recipe.redactor,
            &budget,
        )
        .expect("metadata composition");
        assert_eq!(runtime.endpoints.len(), count);
        assert!(runtime.pending.is_empty());
        runtime.shutdown().await.expect("inert closure");
        budget.close().expect("no activation slots consumed");
    }
}

#[tokio::test]
async fn development_approval_is_generation_local_and_uses_the_prepared_identity() {
    use rw_ext::ApprovalStore as _;
    let fixture = Fixture::with_approval(
        Arc::new(PluginActivationBudget::default()),
        ActivationApproval::SessionDevelopment,
    );
    let store = PrivatePluginApprovalStore::open(fixture.root.path()).expect("persistent store");
    assert!(
        store
            .approved_fingerprint("lazy")
            .expect("lookup")
            .is_none()
    );
    let waiter = fixture.connect();
    fixture.launcher.admitted.notified().await;
    fixture.launcher.release.add_permits(1);
    assert!(waiter.await.expect("waiter").is_ok());
    assert_eq!(fixture.launcher.inner.launches.load(Ordering::Acquire), 1);
    assert!(
        store
            .approved_fingerprint("lazy")
            .expect("lookup")
            .is_none()
    );
    fixture.endpoint.close().await.expect("retired");
    fixture.budget.close().expect("all capacity returned");
}

#[tokio::test]
async fn rejected_activation_does_not_close_another_ready_generation() {
    let budget = Arc::new(PluginActivationBudget::default());
    let first = Fixture::with_budget(Arc::clone(&budget));
    let second = Fixture::with_budget(Arc::clone(&budget));
    first.launcher.release.add_permits(1);
    first
        .connect()
        .await
        .expect("first waiter")
        .expect("first ready");
    second.launcher.inner.launches.store(1, Ordering::Release);
    second.launcher.release.add_permits(1);
    assert!(second.connect().await.expect("rejected waiter").is_err());
    assert_eq!(first.process.waited.load(Ordering::Acquire), 0);
    first
        .connect()
        .await
        .expect("existing waiter")
        .expect("still ready");
    second.endpoint.close().await.expect("rejection settled");
    first.endpoint.close().await.expect("first retired");
    budget.close().expect("all capacity returned");
}
