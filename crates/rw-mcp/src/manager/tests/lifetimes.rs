use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Semaphore;

struct Work {
    started: Notify,
    release: Semaphore,
    finished: AtomicUsize,
    abandoned: AtomicUsize,
}
impl Default for Work {
    fn default() -> Self {
        Self {
            started: Notify::new(),
            release: Semaphore::new(0),
            finished: AtomicUsize::new(0),
            abandoned: AtomicUsize::new(0),
        }
    }
}
impl Work {
    async fn run(&self) {
        let mut guard = WorkGuard {
            work: self,
            complete: false,
        };
        self.started.notify_one();
        self.release.acquire().await.expect("release work").forget();
        self.finished.fetch_add(1, Ordering::SeqCst);
        guard.complete = true;
    }
}
struct WorkGuard<'a> {
    work: &'a Work,
    complete: bool,
}
impl Drop for WorkGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.work.abandoned.fetch_add(1, Ordering::SeqCst);
        }
    }
}

pub(super) struct ControlledClient {
    invocation: Work,
    closing: Work,
    block_close: AtomicBool,
    pub(super) closed: AtomicBool,
    catalogs: AtomicUsize,
}
impl Default for ControlledClient {
    fn default() -> Self {
        Self {
            invocation: Work::default(),
            closing: Work::default(),
            block_close: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            catalogs: AtomicUsize::new(0),
        }
    }
}
#[async_trait]
impl McpClient for ControlledClient {
    fn catalog_valid(&self) -> bool {
        true
    }

    async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        self.catalogs.fetch_add(1, Ordering::SeqCst);
        Ok(vec![json!({"name":"work","inputSchema":{"type":"object"}})])
    }
    async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
        Ok(vec![])
    }
    async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
        Ok(vec![])
    }
    async fn call_tool(&self, _: &str, arguments: Value) -> Result<Value, McpError> {
        self.invocation.run().await;
        Ok(arguments)
    }
    async fn read_resource(&self, _: &str) -> Result<Value, McpError> {
        self.invocation.run().await;
        Ok(json!({}))
    }
    async fn get_prompt(&self, _: &str, _: Value) -> Result<Value, McpError> {
        self.invocation.run().await;
        Ok(json!({}))
    }
    async fn close(&self, _: Duration) -> Result<(), McpError> {
        if self.block_close.load(Ordering::SeqCst) {
            self.closing.run().await;
        }
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
pub(super) struct ControlledConnector {
    pub(super) client: Arc<ControlledClient>,
    connecting: Work,
    block: AtomicBool,
    calls: AtomicUsize,
}
#[async_trait]
impl McpConnector for ControlledConnector {
    async fn connect(&self, _: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.block.load(Ordering::SeqCst) {
            self.connecting.run().await;
        }
        Ok(self.client.clone())
    }
}
pub(super) fn config(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: McpServerId::new(id).expect("id"),
        transport: McpTransportConfig::Stdio {
            executable: "fixture".into(),
            args: vec![],
            working_directory: None,
            environment: vec![],
            sandbox: McpStdioSandboxPolicy::default(),
        },
        enabled: false,
        defer_tools: true,
        tool_capabilities: crate::McpToolCapabilityOverrides::default(),
    }
}
pub(super) async fn fixture(
    block_connect: bool,
) -> (McpManager, Arc<ControlledConnector>, McpServerId) {
    let connector = Arc::new(ControlledConnector {
        client: Arc::new(ControlledClient::default()),
        connecting: Work::default(),
        block: AtomicBool::new(block_connect),
        calls: AtomicUsize::new(0),
    });
    let manager = McpManager::new(
        connector.clone(),
        Arc::new(MemorySpool::default()),
        Arc::new(CompactJsonEncoder),
        McpLimits {
            shutdown_timeout: Duration::from_millis(30),
            request_timeout: Duration::from_secs(3),
            ..McpLimits::default()
        },
    );
    let config = config("owned");
    let id = config.id.clone();
    manager.register(config).await.expect("register");
    if !block_connect {
        manager.set_enabled(&id, true).await.expect("connect");
    }
    (manager, connector, id)
}

#[tokio::test(start_paused = true)]
async fn abandoned_invocations_keep_effects_owned_and_refuse_new_work_until_retirement() {
    for kind in ["tool", "resource", "prompt"] {
        let (manager, connector, id) = fixture(false).await;
        let worker = manager.clone();
        let server = id.clone();
        let caller = tokio::spawn(async move {
            match kind {
                "tool" => worker.call_tool(&server, "work", json!({})).await,
                "resource" => worker.read_resource(&server, "fixture://resource").await,
                _ => worker.get_prompt(&server, "work", json!({})).await,
            }
        });
        connector.client.invocation.started.notified().await;
        caller.abort();
        assert!(caller.await.expect_err("caller cancelled").is_cancelled());
        assert!(matches!(
            manager.settle_effects().await,
            Err(McpError::EffectsUnsettled { .. })
        ));
        assert_eq!(
            connector.client.invocation.abandoned.load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            connector.client.invocation.finished.load(Ordering::SeqCst),
            0
        );
        assert!(manager.call_tool(&id, "work", json!({})).await.is_err());
        connector.client.invocation.release.add_permits(1);
        manager
            .settle_effects()
            .await
            .expect("physical effect and close settle");
        assert_eq!(
            connector.client.invocation.finished.load(Ordering::SeqCst),
            1
        );
        assert!(connector.client.closed.load(Ordering::SeqCst));
        assert!(matches!(
            manager.statuses().await[0].state,
            ServerState::Failed { .. }
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn invocation_deadline_reports_unsettled_without_dropping_actual_work() {
    let (manager, connector, id) = fixture(false).await;
    let worker = manager.clone();
    let server = id.clone();
    let caller = tokio::spawn(async move { worker.call_tool(&server, "work", json!({})).await });
    connector.client.invocation.started.notified().await;
    assert!(matches!(
        caller.await.expect("caller returns"),
        Err(McpError::EffectsUnsettled { .. })
    ));
    assert_eq!(
        connector.client.invocation.abandoned.load(Ordering::SeqCst),
        0
    );
    connector.client.invocation.release.add_permits(1);
    manager
        .settle_effects()
        .await
        .expect("deadline work eventually retires");
}

#[tokio::test(start_paused = true)]
async fn cancelled_connection_waiter_cannot_drop_or_duplicate_the_connector() {
    let (manager, connector, id) = fixture(true).await;
    let worker = manager.clone();
    let server = id.clone();
    let caller = tokio::spawn(async move { worker.set_enabled(&server, true).await });
    connector.connecting.started.notified().await;
    caller.abort();
    assert!(caller.await.expect_err("cancelled waiter").is_cancelled());
    assert!(manager.settle_effects().await.is_err());
    assert_eq!(connector.connecting.abandoned.load(Ordering::SeqCst), 0);
    let worker = manager.clone();
    let server = id.clone();
    let second = tokio::spawn(async move { worker.set_enabled(&server, true).await });
    tokio::task::yield_now().await;
    assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    connector.connecting.release.add_permits(1);
    assert!(second.await.expect("second waiter settles").is_err());
    manager
        .settle_effects()
        .await
        .expect("connector and late client retire");
    assert!(connector.client.closed.load(Ordering::SeqCst));
    assert_eq!(connector.client.catalogs.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn concurrent_enable_waiters_share_one_connection_attempt() {
    let (manager, connector, id) = fixture(true).await;
    let worker = manager.clone();
    let server = id.clone();
    let first = tokio::spawn(async move { worker.set_enabled(&server, true).await });
    connector.connecting.started.notified().await;
    let worker = manager.clone();
    let server = id.clone();
    let second = tokio::spawn(async move { worker.set_enabled(&server, true).await });
    tokio::task::yield_now().await;
    first.abort();
    assert!(first.await.expect_err("first cancelled").is_cancelled());
    connector.connecting.release.add_permits(1);
    second
        .await
        .expect("shared waiter")
        .expect("shared connection");
    assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(manager.statuses().await[0].state, ServerState::Ready);
}

#[tokio::test(start_paused = true)]
async fn shutdown_survives_a_dropped_waiter_and_closes_admission_permanently() {
    let (manager, connector, id) = fixture(false).await;
    connector.client.block_close.store(true, Ordering::SeqCst);
    let worker = manager.clone();
    let first = tokio::spawn(async move { worker.shutdown().await });
    connector.client.closing.started.notified().await;
    first.abort();
    assert!(
        first
            .await
            .expect_err("shutdown waiter cancelled")
            .is_cancelled()
    );
    assert!(manager.set_enabled(&id, true).await.is_err());
    assert!(manager.register(config("another")).await.is_err());
    assert_eq!(connector.client.closing.abandoned.load(Ordering::SeqCst), 0);
    connector.client.closing.release.add_permits(1);
    assert!(
        manager
            .shutdown()
            .await
            .iter()
            .all(|(_, result)| result.is_ok())
    );
    assert_eq!(connector.client.closing.finished.load(Ordering::SeqCst), 1);
    assert!(manager.set_enabled(&id, true).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn disabling_waits_for_actual_invocations_even_after_client_close_returns() {
    let (manager, connector, id) = fixture(false).await;
    let worker = manager.clone();
    let server = id.clone();
    let caller = tokio::spawn(async move { worker.call_tool(&server, "work", json!({})).await });
    connector.client.invocation.started.notified().await;
    assert!(matches!(
        manager.set_enabled(&id, false).await,
        Err(McpError::EffectsUnsettled { .. })
    ));
    assert!(connector.client.closed.load(Ordering::SeqCst));
    assert_eq!(
        connector.client.invocation.abandoned.load(Ordering::SeqCst),
        0
    );
    assert!(manager.set_enabled(&id, true).await.is_err());
    connector.client.invocation.release.add_permits(1);
    manager
        .settle_effects()
        .await
        .expect("retirement owns all invoked effects");
    assert!(caller.await.expect("caller joins").is_err());
    assert_eq!(manager.statuses().await[0].state, ServerState::Disabled);
}

#[tokio::test(start_paused = true)]
async fn aggregate_invocation_admission_is_bounded_before_remote_work_starts() {
    let (manager, connector, id) = fixture(false).await;
    let mut callers = Vec::new();
    for _ in 0..super::super::operations::MAX_OWNED_OPERATIONS {
        let worker = manager.clone();
        let server = id.clone();
        callers.push(tokio::spawn(async move {
            worker.call_tool(&server, "work", json!({})).await
        }));
        connector.client.invocation.started.notified().await;
    }
    assert!(matches!(
        manager.call_tool(&id, "work", json!({})).await,
        Err(McpError::Policy(_))
    ));
    let count = callers.len();
    for caller in callers {
        caller.abort();
        assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    }
    connector.client.invocation.release.add_permits(count);
    manager
        .settle_effects()
        .await
        .expect("all owned requests retire");
    assert_eq!(
        connector.client.invocation.finished.load(Ordering::SeqCst),
        count
    );
    assert_eq!(
        connector.client.invocation.abandoned.load(Ordering::SeqCst),
        0
    );
}
