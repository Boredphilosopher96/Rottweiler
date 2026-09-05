#![cfg(test)]

use crate::engine::builtin_hook_dispatcher;
use crate::engine::model;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::project_session_events;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::turn;
use crate::engine::turn::append_thinking;
use rw_providers::FinishReason;
use rw_providers::ProviderEvent;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::Role;
use rw_types::Turn;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use tempfile::TempDir;

#[test]
fn adjacent_reasoning_deltas_coalesce_and_keep_the_final_signature() {
    let mut blocks = Vec::new();
    append_thinking(&mut blocks, "checking ", None);
    append_thinking(&mut blocks, "the workspace", None);
    append_thinking(&mut blocks, "", Some("opaque-final".to_owned()));

    assert_eq!(
        blocks,
        vec![Block::Thinking {
            content: "checking the workspace".to_owned(),
            signature: Some("opaque-final".to_owned()),
        }]
    );

    append_thinking(&mut blocks, "next item", None);
    assert_eq!(blocks.len(), 2);
}

#[tokio::test]
async fn final_reasoning_signature_is_durable_and_recovers_with_partial_content() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let script = vec![
        Ok(ProviderEvent::MessageStart {
            model: "fixture-model".to_owned(),
        }),
        Ok(ProviderEvent::ThinkingDelta {
            content: "checking the workspace".to_owned(),
            signature: None,
        }),
        Ok(ProviderEvent::ThinkingDelta {
            content: String::new(),
            signature: Some("opaque-final".to_owned()),
        }),
        Ok(ProviderEvent::Finished {
            reason: FinishReason::Stop,
        }),
    ];
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::new([script])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.title = Some("reasoning fixture".to_owned());
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("run").await.expect("message");
    collect_turn(&mut events).await;

    let persisted = sink.events.lock().expect("event sink").clone();
    let signature_index = persisted
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                PendingEvent::ThinkingDelta { content, signature: Some(signature), .. }
                    if content.is_empty() && signature == "opaque-final"
            )
        })
        .expect("final reasoning signature must be journaled");
    let prefix = persisted[..=signature_index]
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    let recovered = project_session_events(&prefix).expect("project signed partial turn");
    assert!(matches!(
        recovered.conversation.last(),
        Some(Turn { role: Role::Assistant, blocks, .. })
            if matches!(blocks.as_slice(), [Block::Thinking { content, signature: Some(signature) }]
                if content == "checking the workspace" && signature == "opaque-final")
    ));
}
