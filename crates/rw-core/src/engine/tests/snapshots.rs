#![cfg(test)]
use crate::engine::tests::fixtures::{
    models::M3Model,
    support::{config, text_turn},
};
use crate::engine::{SessionActor, builtin_hook_dispatcher};
use rw_tools::ToolRegistry;
use rw_types::{Role, config::PermissionDecision};
use std::sync::Arc;

#[tokio::test]
async fn session_snapshot_reports_history_metadata_without_returning_bodies() {
    let root = tempfile::tempdir().expect("workspace");
    let mut config = config(
        root.path(),
        Arc::new(M3Model::new([])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    config.recovered.conversation = (0..16)
        .map(|_| text_turn(Role::User, "x".repeat(256 * 1024)))
        .collect();
    let mut resolved = text_turn(Role::Assistant, "answer");
    resolved.meta.model = Some("provider/model".into());
    config.recovered.conversation.push(resolved);
    let mut alias_only = text_turn(Role::Assistant, "another answer");
    alias_only.meta.model = Some("fast".into());
    config.recovered.conversation.push(alias_only);
    let handle = SessionActor::spawn(config).expect("actor");
    let snapshot = handle.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.conversation_turns, 18);
    assert_eq!(snapshot.resolved_model.as_deref(), Some("provider/model"));
    assert_eq!(snapshot.model_alias, "fast");
    assert!(!snapshot.running);
    handle.close().await.expect("closed");
}
