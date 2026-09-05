use super::*;
use std::sync::atomic::AtomicUsize;

struct FailingClient {
    panic_close: bool,
    fail_catalog: bool,
    closed: AtomicUsize,
}
#[async_trait]
impl McpClient for FailingClient {
    fn catalog_valid(&self) -> bool {
        true
    }
    async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        if self.fail_catalog {
            Err(McpError::Policy("catalog rejected".into()))
        } else {
            Ok(Vec::new())
        }
    }
    async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
        Ok(Vec::new())
    }
    async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
        Ok(Vec::new())
    }
    async fn call_tool(&self, _: &str, _: Value) -> Result<Value, McpError> {
        Ok(Value::Null)
    }
    async fn read_resource(&self, _: &str) -> Result<Value, McpError> {
        Ok(Value::Null)
    }
    async fn get_prompt(&self, _: &str, _: Value) -> Result<Value, McpError> {
        Ok(Value::Null)
    }
    async fn close(&self, _: Duration) -> Result<(), McpError> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_close,
            "fixture close panic after accepting retirement"
        );
        Err(McpError::Policy(
            "fixture cannot prove effects settled".into(),
        ))
    }
}
struct SingleClient {
    client: Mutex<Option<Arc<FailingClient>>>,
    connects: AtomicUsize,
}
#[async_trait]
impl McpConnector for SingleClient {
    async fn connect(&self, _: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.client
            .lock()
            .await
            .take()
            .map(|client| client as Arc<dyn McpClient>)
            .ok_or_else(|| McpError::Policy("duplicate connection".into()))
    }
}

#[tokio::test]
async fn failed_or_panicked_retirement_retains_actual_client_and_rejects_reconnection() {
    for (panic_close, fail_catalog) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let client = Arc::new(FailingClient {
            panic_close,
            fail_catalog,
            closed: AtomicUsize::new(0),
        });
        let actual = Arc::downgrade(&client);
        let connector = Arc::new(SingleClient {
            client: Mutex::new(Some(client)),
            connects: AtomicUsize::new(0),
        });
        let manager = McpManager::new(
            connector.clone(),
            Arc::new(MemorySpool::default()),
            Arc::new(CompactJsonEncoder),
            McpLimits::default(),
        );
        let config = super::lifetimes::config("unproven");
        let id = config.id.clone();
        manager.register(config).await.expect("register");
        let result = manager.set_enabled(&id, true).await;
        if fail_catalog {
            assert!(matches!(result, Err(McpError::EffectsUnsettled { .. })));
        } else {
            result.expect("connect");
            assert!(matches!(
                manager.set_enabled(&id, false).await,
                Err(McpError::EffectsUnsettled { .. })
            ));
        }
        assert!(manager.set_enabled(&id, true).await.is_err());
        assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
        drop(manager);
        drop(connector);
        let retained = actual
            .upgrade()
            .expect("failed proof retains actual client");
        assert_eq!(retained.closed.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn shutdown_attempts_every_server_even_when_one_cannot_begin_retirement() {
    let (manager, connector, id) = super::lifetimes::fixture(false).await;
    let first = super::lifetimes::config("before-owned");
    let first_id = first.id.clone();
    manager
        .register(first)
        .await
        .expect("register exhausted server");
    manager
        .inner
        .servers
        .write()
        .await
        .get_mut(&first_id)
        .expect("server")
        .generation = u64::MAX;
    assert!(
        manager
            .shutdown()
            .await
            .iter()
            .all(|(_, result)| result.is_err())
    );
    assert!(connector.client.closed.load(Ordering::SeqCst));
    assert_eq!(
        manager
            .inner
            .servers
            .read()
            .await
            .get(&id)
            .expect("owned server")
            .state,
        ServerState::Disabled
    );
}
