#![allow(clippy::expect_used)]
mod inbound;
mod lifetimes;

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify};

use super::*;
use crate::{McpStdioSandboxPolicy, McpTransportConfig, OverflowReference};

struct MockClient {
    schema_version: Arc<Mutex<u8>>,
    closed: AtomicBool,
    fail_close: bool,
    invalidated: AtomicBool,
}

#[async_trait]
impl McpClient for MockClient {
    fn catalog_valid(&self) -> bool {
        !self.invalidated.load(Ordering::Acquire)
    }

    async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let version = *self.schema_version.lock().await;
        Ok(vec![
            json!({"name":"lookup","description":"Look up records\nwithout loading this large schema.","inputSchema":{"type":"object","properties":{"version":{"const":version},"query":{"type":"string"}}}}),
        ])
    }
    async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
        Ok(vec![
            json!({"name":"guide","uri":"memory://guide","description":"Guide"}),
        ])
    }
    async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
        Ok(vec![
            json!({"name":"review","description":"Review a change"}),
        ])
    }
    async fn call_tool(&self, _name: &str, arguments: Value) -> Result<Value, McpError> {
        Ok(arguments)
    }
    async fn read_resource(&self, uri: &str) -> Result<Value, McpError> {
        Ok(json!({"uri":uri,"text":"resource"}))
    }
    async fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        Ok(json!({"name":name,"arguments":arguments}))
    }
    async fn close(&self, _timeout: Duration) -> Result<(), McpError> {
        self.closed.store(true, Ordering::Release);
        if self.fail_close {
            Err(McpError::Protocol("fixture close failed".to_owned()))
        } else {
            Ok(())
        }
    }
}

struct MockConnector {
    clients: Mutex<BTreeMap<McpServerId, Arc<MockClient>>>,
}

struct BlockingConnector {
    client: Arc<MockClient>,
    started: Notify,
    proceed: Notify,
}

#[async_trait]
impl McpConnector for BlockingConnector {
    async fn connect(&self, _config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
        self.started.notify_one();
        self.proceed.notified().await;
        Ok(self.client.clone())
    }
}
#[async_trait]
impl McpConnector for MockConnector {
    async fn connect(&self, config: &McpServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
        self.clients
            .lock()
            .await
            .get(&config.id)
            .cloned()
            .map(|client| client as Arc<dyn McpClient>)
            .ok_or_else(|| McpError::UnknownServer(config.id.clone()))
    }
}

#[derive(Default)]
struct MemorySpool {
    values: Mutex<Vec<Vec<u8>>>,
}
#[async_trait]
impl OverflowSpool for MemorySpool {
    async fn write(
        &self,
        server: &McpServerId,
        _operation: &str,
        bytes: &[u8],
    ) -> Result<OverflowReference, McpError> {
        self.values.lock().await.push(bytes.to_vec());
        Ok(OverflowReference {
            id: format!("opaque-{server}"),
            bytes: bytes.len(),
        })
    }
    async fn read(&self, reference: &OverflowReference) -> Result<Vec<u8>, McpError> {
        self.values
            .lock()
            .await
            .iter()
            .find(|value| value.len() == reference.bytes)
            .cloned()
            .ok_or_else(|| McpError::Spool("missing".to_owned()))
    }
    async fn remove(&self, _reference: &OverflowReference) -> Result<(), McpError> {
        Ok(())
    }
}

#[tokio::test]
async fn five_servers_stay_deferred_and_support_full_catalog_and_calls() {
    let mut clients = BTreeMap::new();
    for index in 0..5 {
        let id = McpServerId::new(format!("server-{index}")).expect("id");
        clients.insert(
            id,
            Arc::new(MockClient {
                schema_version: Arc::new(Mutex::new(1)),
                closed: AtomicBool::new(false),
                fail_close: false,
                invalidated: AtomicBool::new(false),
            }),
        );
    }
    let connector = Arc::new(MockConnector {
        clients: Mutex::new(clients),
    });
    let spool = Arc::new(MemorySpool::default());
    let manager = McpManager::new(
        connector,
        spool,
        Arc::new(CompactJsonEncoder),
        McpLimits {
            response_bytes: 128,
            request_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
        },
    );
    for index in 0..5 {
        manager
            .register(McpServerConfig {
                id: McpServerId::new(format!("server-{index}")).expect("id"),
                transport: McpTransportConfig::Stdio {
                    executable: "fixture".into(),
                    args: Vec::new(),
                    working_directory: None,
                    environment: Vec::new(),
                    sandbox: McpStdioSandboxPolicy::default(),
                },
                enabled: true,
                defer_tools: true,
                tool_capabilities: crate::McpToolCapabilityOverrides::default(),
            })
            .await
            .expect("register");
    }
    assert!(
        manager
            .connect_all()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
    let prompt = manager.deferred_prompt().await.expect("prompt");
    let tokenizer = tiktoken_rs::cl100k_base().expect("tokenizer");
    assert!(tokenizer.encode_with_special_tokens(&prompt).len() < 2_000);
    let index_json = serde_json::to_value(manager.deferred_tool_index().await).expect("index");
    assert!(index_json.to_string().find("inputSchema").is_none());
    let definitions = manager.tool_search("look", None).await;
    assert_eq!(definitions.len(), 5);
    assert_eq!(definitions[0].capabilities.capabilities().len(), 2);
    assert_eq!(manager.resources().await.len(), 5);
    assert_eq!(manager.prompts().await.len(), 5);
    let server = McpServerId::new("server-0").expect("id");
    assert!(
        !manager
            .call_tool(&server, "lookup", json!({"small":true}))
            .await
            .expect("call")
            .truncated
    );
    assert_eq!(
        manager
            .read_resource(&server, "memory://guide")
            .await
            .expect("resource")
            .format,
        "json"
    );
    assert!(
        manager
            .get_prompt(&server, "review", json!({"large":"x".repeat(512)}))
            .await
            .expect("prompt")
            .truncated
    );
    assert!(
        manager
            .shutdown()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
}

#[tokio::test]
async fn changed_schema_stays_inactive_until_approval() {
    let id = McpServerId::new("mutable").expect("id");
    let schema_version = Arc::new(Mutex::new(1));
    let client = Arc::new(MockClient {
        schema_version: Arc::clone(&schema_version),
        closed: AtomicBool::new(false),
        fail_close: false,
        invalidated: AtomicBool::new(false),
    });
    let connector = Arc::new(MockConnector {
        clients: Mutex::new(BTreeMap::from([(id.clone(), client)])),
    });
    let manager = McpManager::new(
        connector,
        Arc::new(MemorySpool::default()),
        Arc::new(CompactJsonEncoder),
        McpLimits::default(),
    );
    manager
        .register(McpServerConfig {
            id: id.clone(),
            transport: McpTransportConfig::Stdio {
                executable: "fixture".into(),
                args: vec![],
                working_directory: None,
                environment: vec![],
                sandbox: McpStdioSandboxPolicy::default(),
            },
            enabled: true,
            defer_tools: true,
            tool_capabilities: crate::McpToolCapabilityOverrides::default(),
        })
        .await
        .expect("register");
    assert!(manager.connect_all().await[0].1.is_ok());
    *schema_version.lock().await = 2;
    assert!(manager.refresh_tools(&id, false).await.expect("refresh"));
    assert!(matches!(
        manager.call_tool(&id, "lookup", json!({})).await,
        Err(McpError::NotConnected(_))
    ));
    assert!(manager.tool_search("lookup", Some(&id)).await.is_empty());
    assert!(manager.approve_pending_tools(&id).await.expect("approve"));
    assert_eq!(
        manager.tool_search("lookup", Some(&id)).await[0].input_schema["properties"]["version"]["const"],
        2
    );
    manager.set_enabled(&id, false).await.expect("disable");
    *schema_version.lock().await = 3;
    manager.set_enabled(&id, true).await.expect("re-enable");
    assert!(matches!(
        manager.call_tool(&id, "lookup", json!({})).await,
        Err(McpError::NotConnected(_))
    ));
    assert!(manager.tool_search("lookup", Some(&id)).await.is_empty());
    assert!(
        manager
            .approve_pending_tools(&id)
            .await
            .expect("approve reconnect")
    );
    assert_eq!(
        manager.tool_search("lookup", Some(&id)).await[0].input_schema["properties"]["version"]["const"],
        3
    );
}

#[tokio::test]
async fn failed_close_does_not_make_an_explicitly_disabled_server_reconnectable() {
    let id = McpServerId::new("close-failure").expect("id");
    let client = Arc::new(MockClient {
        schema_version: Arc::new(Mutex::new(1)),
        closed: AtomicBool::new(false),
        fail_close: true,
        invalidated: AtomicBool::new(false),
    });
    let connector = Arc::new(MockConnector {
        clients: Mutex::new(BTreeMap::from([(id.clone(), client)])),
    });
    let manager = McpManager::new(
        connector,
        Arc::new(MemorySpool::default()),
        Arc::new(CompactJsonEncoder),
        McpLimits::default(),
    );
    manager
        .register(McpServerConfig {
            id: id.clone(),
            transport: McpTransportConfig::Stdio {
                executable: "fixture".into(),
                args: vec![],
                working_directory: None,
                environment: vec![],
                sandbox: McpStdioSandboxPolicy::default(),
            },
            enabled: true,
            defer_tools: true,
            tool_capabilities: crate::McpToolCapabilityOverrides::default(),
        })
        .await
        .expect("register");
    assert!(manager.connect_all().await[0].1.is_ok());
    manager
        .set_enabled(&id, false)
        .await
        .expect_err("fixture close fails");
    assert!(!manager.reconnect_if_failed(&id).await.expect("retry gate"));
    let status = manager.statuses().await;
    assert!(!status[0].enabled);
    assert!(matches!(status[0].state, ServerState::Failed { .. }));
}

#[tokio::test]
async fn disable_during_connect_cannot_resurrect_stale_generation() {
    let id = McpServerId::new("racing").expect("id");
    let connector = Arc::new(BlockingConnector {
        client: Arc::new(MockClient {
            schema_version: Arc::new(Mutex::new(1)),
            closed: AtomicBool::new(false),
            fail_close: false,
            invalidated: AtomicBool::new(false),
        }),
        started: Notify::new(),
        proceed: Notify::new(),
    });
    let manager = Arc::new(McpManager::new(
        connector.clone(),
        Arc::new(MemorySpool::default()),
        Arc::new(CompactJsonEncoder),
        McpLimits::default(),
    ));
    manager
        .register(McpServerConfig {
            id: id.clone(),
            transport: McpTransportConfig::Stdio {
                executable: "fixture".into(),
                args: vec![],
                working_directory: None,
                environment: vec![],
                sandbox: McpStdioSandboxPolicy::default(),
            },
            enabled: true,
            defer_tools: true,
            tool_capabilities: crate::McpToolCapabilityOverrides::default(),
        })
        .await
        .expect("register");
    let running = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.connect_all().await })
    };
    connector.started.notified().await;
    let disabling = {
        let manager = manager.clone();
        let id = id.clone();
        tokio::spawn(async move { manager.set_enabled(&id, false).await })
    };
    tokio::task::yield_now().await;
    assert!(!disabling.is_finished());
    connector.proceed.notify_one();
    disabling
        .await
        .expect("disable joins")
        .expect("disable settles");
    let _ = running.await.expect("join");
    let status = manager.statuses().await.remove(0);
    assert!(!status.enabled);
    assert_eq!(status.state, ServerState::Disabled);
    assert_eq!(status.tool_count, 0);
}
