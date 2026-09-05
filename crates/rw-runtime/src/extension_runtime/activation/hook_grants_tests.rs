#![allow(clippy::expect_used)]
use crate::extension_runtime::{PluginRuntimeBudget, SharedPluginRedactor, tests::rollback_plugin};
use rw_ext::{HookClass, HookDispatcher, HookEffect, HookEvent, HookFailurePolicy};
use rw_plugin_protocol::{PluginHookCapability, PluginToolCapability, PluginToolEffect};
use std::sync::Arc;

#[tokio::test]
async fn native_hooks_do_not_receive_sibling_tool_effect_authority() {
    let root = tempfile::tempdir().expect("fixture root");
    let budget = Arc::new(PluginRuntimeBudget::default());
    let redactor = Arc::new(SharedPluginRedactor::new(
        rw_providers::FixtureRedactor::default(),
    ));
    for (index, (event, class, writes)) in [
        (HookEvent::PreTool, HookClass::Policy, true),
        (HookEvent::PostTool, HookClass::Transform, true),
        (HookEvent::PreTool, HookClass::Observer, true),
        (HookEvent::SessionStart, HookClass::Policy, true),
        (HookEvent::SessionEnd, HookClass::Observer, true),
        (HookEvent::SessionStart, HookClass::Observer, false),
    ]
    .into_iter()
    .enumerate()
    {
        let (config, mut manifest) = rollback_plugin(root.path(), &format!("grants_{index}"));
        manifest.capabilities.tools.push(PluginToolCapability {
            name: "sibling_tool".to_owned(),
            description: "One tool declares host-mediated filesystem effects".to_owned(),
            schema: serde_json::json!({"type":"object"}),
            caps: if writes {
                vec![PluginToolEffect::WritesFilesystem]
            } else {
                vec![]
            },
        });
        manifest.capabilities.hooks.push(PluginHookCapability {
            name: event,
            class,
            failure_policy: if class == HookClass::Observer {
                HookFailurePolicy::FailOpen
            } else {
                HookFailurePolicy::FailClosed
            },
        });
        std::fs::write(
            &config.manifest_path,
            serde_json::to_vec(&manifest).expect("manifest JSON"),
        )
        .expect("manifest");
        let owner = crate::extension_runtime::generations::PluginGenerationOwner::compose(
            crate::extension_runtime::generations::PluginGenerationConfig {
                private_root: root.path().to_path_buf(),
                helper: root.path().join("unavailable-helper"),
                redactor: redactor.clone(),
                budget: budget.clone(),
                session_ui: Arc::new(crate::extension_runtime::ui::UiSessionBudget::default()),
            },
            &[config],
            &[root.path().to_path_buf()],
        )
        .expect("inert registration");
        let runtime = owner.current();
        let mut dispatcher = HookDispatcher::new();
        let (registration, handler) = runtime.hooks[0].clone();
        assert_eq!(registration.effect(), HookEffect::ReadOnly);
        assert!(
            !registration
                .required_capabilities()
                .contains(&rw_types::ToolCapability::WriteFilesystem)
        );
        dispatcher
            .register_shared(registration, handler)
            .expect("code-only hook registration");
        assert!(!dispatcher.has_workspace_mutating_tool_hook(event, "unrelated_tool"));
        owner.shutdown().await.expect("inert shutdown");
    }
    budget
        .close()
        .expect("registration starts no native workers");
}
