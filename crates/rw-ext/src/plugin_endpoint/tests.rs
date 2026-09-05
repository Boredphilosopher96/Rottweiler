#![allow(clippy::expect_used)]
use super::*;
use crate::RpcToolAdapter;
use rw_plugin_protocol::{PluginCapabilities, PluginToolEffect};
use rw_tools::{MutationScope, Tool, ToolContext};
use rw_types::ToolCapability;
use serde_json::json;
use std::sync::atomic::AtomicUsize;

struct DormantFixture {
    metadata: PluginEndpointMetadata,
    connections: AtomicUsize,
}
#[async_trait]
impl PluginEndpoint for DormantFixture {
    fn metadata(&self) -> &PluginEndpointMetadata {
        &self.metadata
    }
    async fn connect(&self, _: &CancellationToken) -> Result<PluginConnection, PluginRpcError> {
        self.connections.fetch_add(1, Ordering::AcqRel);
        Err(PluginRpcError {
            code: "approval_required".to_owned(),
            message: "fixture has not been approved".to_owned(),
        })
    }
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        Err(PluginRpcError {
            code: "effects_unsettled".to_owned(),
            message: "seeded owner proof failure".to_owned(),
        })
    }
    async fn close(&self) -> Result<(), PluginRpcError> {
        self.settle_effects().await
    }
}
fn manifest() -> PluginManifest {
    PluginManifest {
        name: "endpoint-fixture".to_owned(),
        version: "1.0.0".to_owned(),
        protocol: rw_plugin_protocol::PROTOCOL_VERSION,
        capabilities: PluginCapabilities {
            tools: vec![
                PluginToolCapability {
                    name: "fixture_tool".to_owned(),
                    description: "fixture".to_owned(),
                    schema: json!({"type":"object"}),
                    caps: vec![PluginToolEffect::ReadsFilesystem],
                },
                PluginToolCapability {
                    name: "fixture_write".to_owned(),
                    description: "fixture".to_owned(),
                    schema: json!({"type":"object"}),
                    caps: vec![PluginToolEffect::WritesFilesystem],
                },
            ],
            ..PluginCapabilities::default()
        },
    }
}

#[tokio::test]
async fn registration_is_inert_and_invocation_obtains_its_connection() {
    let manifest = manifest();
    let declaration = manifest.capabilities.tools[0].clone();
    let endpoint = Arc::new(DormantFixture {
        metadata: PluginEndpointMetadata::new(manifest).expect("valid metadata"),
        connections: AtomicUsize::new(0),
    });
    let adapter = RpcToolAdapter::new(declaration.clone(), endpoint.clone()).expect("declaration");
    assert_eq!(adapter.descriptor().name, "fixture_tool");
    assert!(
        adapter
            .descriptor()
            .capabilities
            .contains(&ToolCapability::WriteFilesystem)
    );
    assert_eq!(
        adapter.mutation_scope(&json!({})),
        MutationScope::OpaqueWorkspace
    );
    assert_eq!(endpoint.connections.load(Ordering::Acquire), 0);
    let mut changed = declaration;
    changed.description = "changed after registration".to_owned();
    assert!(RpcToolAdapter::new(changed, endpoint.clone()).is_err());
    assert_eq!(endpoint.connections.load(Ordering::Acquire), 0);
    let root = tempfile::tempdir().expect("context root");
    let context = ToolContext::new(root.path()).expect("context");
    assert!(
        adapter
            .execute(&context, json!({}))
            .await
            .expect_err("approval remains mandatory")
            .to_string()
            .contains("not been approved")
    );
    assert_eq!(endpoint.connections.load(Ordering::Acquire), 1);
    assert!(
        adapter
            .settle_effects()
            .await
            .expect_err("owner proof is required")
            .to_string()
            .contains("seeded owner proof failure")
    );
}
