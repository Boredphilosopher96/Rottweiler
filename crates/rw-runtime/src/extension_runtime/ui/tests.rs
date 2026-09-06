#![allow(clippy::expect_used)]
use super::{RuntimeUiRegistry, SharedPluginRedactor, UiBudget};
use async_trait::async_trait;
use rw_core::ui::UiRegistry;
use rw_ext::{PluginConnection, PluginEndpoint, PluginEndpointMetadata, PluginRpcError};
use rw_plugin_protocol::{PluginCapabilities, PluginCommandCapability, PluginManifest};
use rw_tools::CancellationToken;
use rw_types::extension_ui::{
    UiAction, UiActionRequest, UiActionTarget, UiContribution, UiField, UiProjectedField,
    UiSelectorStep,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Endpoint {
    metadata: PluginEndpointMetadata,
    connections: AtomicUsize,
}
#[async_trait]
impl PluginEndpoint for Endpoint {
    fn metadata(&self) -> &PluginEndpointMetadata {
        &self.metadata
    }
    async fn connect(&self, _: &CancellationToken) -> Result<PluginConnection, PluginRpcError> {
        self.connections.fetch_add(1, Ordering::SeqCst);
        Err(PluginRpcError {
            code: "fixture".into(),
            message: "fixture does not launch".into(),
        })
    }
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        Ok(())
    }
    async fn close(&self) -> Result<(), PluginRpcError> {
        Ok(())
    }
}
fn endpoint(count: usize) -> Arc<Endpoint> {
    let ui = (0..count)
        .map(|index| UiContribution::Panel {
            id: format!("panel-{index}"),
            title: "secret-token".into(),
            fields: vec![UiField::Text {
                id: "text".into(),
                label: "Text".into(),
                path: vec![UiSelectorStep::Field {
                    name: "text".into(),
                }],
            }],
            actions: vec![UiAction {
                id: "open".into(),
                label: "Open".into(),
                command: "open".into(),
                arguments: json!({"view":"details"}),
            }],
        })
        .collect();
    Arc::new(Endpoint {
        metadata: PluginEndpointMetadata::new(PluginManifest {
            name: "ui-fixture".into(),
            version: "1.0.0".into(),
            protocol: 3,
            capabilities: PluginCapabilities {
                ui,
                commands: vec![PluginCommandCapability {
                    name: "open".into(),
                    description: "Open".into(),
                    argument_hint: None,
                    allowed_tools: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .expect("manifest"),
        connections: AtomicUsize::new(0),
    })
}
fn registry() -> (Arc<UiBudget>, RuntimeUiRegistry) {
    let budget = Arc::new(UiBudget::default());
    let registry = RuntimeUiRegistry::new(
        budget.clone(),
        Arc::new(SharedPluginRedactor::new(
            rw_providers::FixtureRedactor::new(["secret-token".into()]),
        )),
        Arc::new(super::UiSessionBudget::default()),
    );
    (budget, registry)
}
#[test]
fn catalog_and_panel_projection_do_not_activate_code_and_preserve_source_bounds() {
    let (budget, registry) = registry();
    let endpoint = endpoint(1);
    let owner = endpoint.metadata.ui_owner();
    registry.register(endpoint.clone()).expect("registered");
    assert_eq!(registry.catalog().expect("catalog").entries.len(), 1);
    assert!(
        !registry.catalog().expect("catalog").entries[0]
            .descriptor
            .title
            .contains("secret-token")
    );
    let revision = registry
        .publish_panel(
            &owner,
            "panel-0",
            json!({"text": format!("{}secret-token", "a".repeat(4090))}),
        )
        .expect("publish");
    let panels = registry.panels().expect("panels");
    panels.validate().expect("bounded panel snapshot");
    assert_eq!(revision, 1);
    let UiProjectedField::Text {
        value: Some(text), ..
    } = &panels.panels[0].presentation.projected.fields[0]
    else {
        panic!("text field")
    };
    assert!(!text.contains("secret"));
    assert_eq!(endpoint.connections.load(Ordering::SeqCst), 0);
    assert!(
        registry
            .publish_panel(&owner, "panel-0", json!({"text":"replacement"}))
            .is_err()
    );
    assert_eq!(registry.panels().expect("unchanged").panels[0].revision, 1);
    registry.close();
    assert!(registry.catalog().is_err());
    budget.close().expect("all registry charges returned");
}
#[test]
fn action_requires_current_generation_panel_revision_and_declared_identity() {
    let (_, registry) = registry();
    let endpoint = endpoint(1);
    let owner = endpoint.metadata.ui_owner();
    registry.register(endpoint.clone()).expect("registered");
    let revision = registry
        .publish_panel(&owner, "panel-0", json!({"text":"hello"}))
        .expect("published");
    let mut request = UiActionRequest {
        owner,
        contribution_id: "panel-0".into(),
        action_id: "open".into(),
        target: UiActionTarget::Panel { revision },
    };
    assert!(registry.resolve_action(&request, None).is_ok());
    request.target = UiActionTarget::Panel { revision: 0 };
    assert!(registry.resolve_action(&request, None).is_err());
    request.target = UiActionTarget::Panel { revision };
    request.owner = super::tests::endpoint(1).metadata.ui_owner();
    assert!(registry.resolve_action(&request, None).is_err());
    assert_eq!(endpoint.connections.load(Ordering::SeqCst), 0);
}
#[test]
fn rejected_registration_does_not_consume_catalog_or_allocation_capacity() {
    let (budget, registry) = registry();
    let first = endpoint(128);
    registry.register(first.clone()).expect("full catalog");
    assert!(registry.register(endpoint(1)).is_err());
    assert!(registry.register(first).is_err());
    assert_eq!(
        registry
            .catalog()
            .expect("full catalog retained")
            .entries
            .len(),
        128
    );
    registry.close();
    budget
        .close()
        .expect("rejected registration charges returned");
}

#[test]
fn session_panel_credits_span_registries_and_allow_replacement_at_capacity() {
    let budget = Arc::new(UiBudget::default());
    let session = Arc::new(super::UiSessionBudget::default());
    let redactor = Arc::new(SharedPluginRedactor::new(
        rw_providers::FixtureRedactor::default(),
    ));
    let registries: Vec<_> = (0..9)
        .map(|_| {
            let registry =
                RuntimeUiRegistry::new(budget.clone(), redactor.clone(), session.clone());
            let endpoint = endpoint(1);
            let owner = endpoint.metadata.ui_owner();
            registry.register(endpoint).expect("registered");
            (registry, owner)
        })
        .collect();
    for (registry, owner) in registries.iter().take(8) {
        registry
            .publish_panel(owner, "panel-0", json!({"text":"first"}))
            .expect("admitted slot");
    }
    assert!(
        registries[8]
            .0
            .publish_panel(&registries[8].1, "panel-0", json!({"text":"ninth"}))
            .is_err()
    );
    registries[0].0.state.lock().expect("state").last_update = None;
    assert_eq!(
        registries[0]
            .0
            .publish_panel(&registries[0].1, "panel-0", json!({"text":"replacement"}))
            .expect("replacement reuses slot"),
        2
    );
    registries[0].0.close();
    registries[8]
        .0
        .publish_panel(&registries[8].1, "panel-0", json!({"text":"released slot"}))
        .expect("slot returned");
    for (registry, _) in &registries {
        registry.close();
    }
    budget.close().expect("all retained allocations returned");
}

#[test]
fn ui_encoded_ceiling_preserves_utf8_escaping_and_limit_error() {
    // Quotes, two UTF-8 bytes, and a two-byte escaped newline.
    let value = "é\n";
    assert_eq!(super::encoded_bytes(&value, 6).expect("exact boundary"), 6);
    let rejected = super::encoded_bytes(&value, 5).expect_err("one byte over the limit");
    assert_eq!(rejected.code, "ui_unavailable");
    assert_eq!(rejected.message, "UI encoded limit");
}
