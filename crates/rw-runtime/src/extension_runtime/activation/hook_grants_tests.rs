#![allow(clippy::expect_used)]
use crate::extension_runtime::{
    PluginRuntimeBudget, PluginSessionRuntime, SharedPluginRedactor, tests::rollback_plugin,
};
use rw_ext::{HookClass, HookDispatcher, HookEffect, HookEvent, HookFailurePolicy};
use rw_plugin_protocol::{PluginHookCapability, PluginToolCapability, PluginToolEffect};
use std::sync::Arc;

#[tokio::test]
async fn native_hook_registration_uses_sibling_tools_process_write_authority() {
    let root = tempfile::tempdir().expect("fixture root");
    let budget = Arc::new(PluginRuntimeBudget::default());
    let redactor = Arc::new(SharedPluginRedactor::new(
        rw_providers::FixtureRedactor::default(),
    ));
    for (index, (event, class, writes, allowed)) in [
        (HookEvent::PreTool, HookClass::Policy, true, true),
        (HookEvent::PostTool, HookClass::Transform, true, true),
        (HookEvent::PreTool, HookClass::Observer, true, false),
        (HookEvent::SessionStart, HookClass::Policy, true, false),
        (HookEvent::SessionEnd, HookClass::Observer, true, false),
        (HookEvent::SessionStart, HookClass::Observer, false, true),
    ]
    .into_iter()
    .enumerate()
    {
        let (config, mut manifest) = rollback_plugin(root.path(), &format!("grants_{index}"));
        manifest.capabilities.tools.push(PluginToolCapability {
            name: "sibling_tool".to_owned(),
            description: "One tool controls the process filesystem grants".to_owned(),
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
        let runtime = PluginSessionRuntime::compose(
            &[config],
            root.path(),
            &[root.path().to_path_buf()],
            &root.path().join("unavailable-helper"),
            &redactor,
            &budget,
        )
        .expect("inert registration");
        let mut dispatcher = HookDispatcher::new();
        let (registration, handler) = runtime.hooks[0].clone();
        assert_eq!(
            registration.effect(),
            if writes {
                HookEffect::WorkspaceMutating
            } else {
                HookEffect::ReadOnly
            }
        );
        assert_eq!(
            registration
                .required_capabilities()
                .contains(&rw_types::ToolCapability::WriteFilesystem),
            writes
        );
        assert_eq!(
            dispatcher.register_shared(registration, handler).is_ok(),
            allowed
        );
        if allowed && writes {
            assert!(dispatcher.has_workspace_mutating_tool_hook(event, "unrelated_tool"));
        }
        runtime.shutdown().await.expect("inert shutdown");
    }
    budget
        .close()
        .expect("registration starts no native workers");
}
