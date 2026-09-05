#![cfg(test)]

use crate::engine::builtin_hook_dispatcher;
use crate::engine::durability::NoopSessionEventSink;
use crate::engine::model::ModelContextMetadata;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::ContextSurgeryAction;
use crate::engine::tests::fixtures::models::M3Model;
use crate::engine::tests::fixtures::models::PendingModel;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::support::TestEventSinkExt;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::text_turn;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use crate::engine::turn;
use crate::engine::turn::prompt_turn;
use rw_context::AssemblyInput;
use rw_context::ContextAssembler;
use rw_context::LocalTokenEstimator;
use rw_context::PRUNED_TOOL_OUTPUT_REPLACEMENT;
use rw_context::ToonPromptEncoder;
use rw_providers::CacheBreakpointSupport;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::Block;
use rw_types::ContextItemId;
use rw_types::ContextItemKind;
use rw_types::EngineEvent;
use rw_types::Role;
use rw_types::ToolCallId;
use rw_types::ToolOutput;
use rw_types::ToolOutputPart;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::config::CompactionConfig;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn context_queries_and_surgery_are_offline_and_actor_consistent() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::default());
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.conversation = vec![text_turn(Role::User, "inspect me")];
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let snapshot = handle.context_snapshot().await.expect("context snapshot");
    assert_eq!(snapshot.items.len(), 1);
    assert!(!snapshot.context_window_known);
    assert_eq!(
        snapshot.context_window_reason.as_deref(),
        Some("provider did not report a context window")
    );
    let item_id = snapshot.items[0].item_id.clone();
    handle.pin_context(item_id.clone()).await.expect("pin");
    let pinned = handle.context_snapshot().await.expect("pinned snapshot");
    assert!(pinned.items[0].state.pinned);
    handle.evict_context(item_id).await.expect("evict");
    let evicted = handle.context_snapshot().await.expect("evicted snapshot");
    assert!(evicted.items[0].state.evicted);
    let dump = handle.dump_prompt(None).await.expect("offline prompt dump");
    assert!(dump.turns.is_empty());
    assert_eq!(model.request_count(), 0);
}

#[test]
fn invalid_resolved_overflow_policy_disables_automatic_compaction() {
    let assembled =
        ContextAssembler::assemble(AssemblyInput::default()).expect("empty context assembles");
    let snapshot = turn::context_snapshot(
        &assembled,
        &[],
        &BTreeMap::new(),
        ModelContextMetadata {
            max_context_tokens: Some(10_000),
            max_output_tokens: Some(2_000),
            cache_breakpoints: None,
        },
        &CompactionConfig {
            reserved_tokens: Some(10_000),
            ..CompactionConfig::default()
        },
        None,
        None,
    );
    assert!(!snapshot.context_window_known);
    assert_eq!(snapshot.usable_tokens, 0);
    assert_eq!(snapshot.reserved_tokens, 0);
    assert_eq!(
        snapshot.context_window_reason.as_deref(),
        Some("explicit reserve 10000 must be smaller than context window 10000")
    );
}

#[tokio::test]
async fn context_inventory_exposes_tools_and_rejects_protected_item_surgery() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::default());
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "inspect",
            vec![],
            StubOutcome::Success(ToolResult::new("unused", Value::Null)),
        )))
        .expect("register tool");
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.initial_session_context = vec![text_turn(Role::System, "protected policy")];
    actor_config.recovered.conversation = vec![Turn {
        role: Role::Tool,
        blocks: vec![
            Block::ToolResult {
                id: ToolCallId("call-inspect".to_owned()),
                output: ToolOutput::Structured {
                    value: json!({"answer": 42}),
                },
                is_error: false,
            },
            Block::ToolResult {
                id: ToolCallId("call-second".to_owned()),
                output: ToolOutput::Text {
                    text: "second result".to_owned(),
                },
                is_error: false,
            },
        ],
        meta: TurnMeta::default(),
    }];
    actor_config.recovered.context_surgery = vec![ContextSurgeryAction {
        item_id: ContextItemId("conversation:0".to_owned()),
        pinned: true,
        effective_after_agent_turn: 0,
    }];
    actor_config
        .recovered
        .pruned_tool_outputs
        .insert("call-inspect".to_owned(), 100);
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");

    let snapshot = handle.context_snapshot().await.expect("context snapshot");
    let system = snapshot
        .items
        .iter()
        .find(|item| item.kind == ContextItemKind::System)
        .expect("system inventory item");
    let tool_schema = snapshot
        .items
        .iter()
        .find(|item| {
            item.item_id.0 == "tool:inspect" && item.kind == ContextItemKind::ToolDefinitions
        })
        .expect("tool schema inventory item");
    assert!(!tool_schema.state.pinned);
    let tool_results = snapshot
        .items
        .iter()
        .filter(|item| item.kind == ContextItemKind::ToolResult)
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 2, "no aggregate tool-turn duplicate");
    let pruned = tool_results
        .iter()
        .find(|item| item.item_id.0 == "tool_result:call-inspect")
        .expect("first tool result");
    assert!(pruned.state.pinned);
    assert!(pruned.state.pruned);
    let second = tool_results
        .iter()
        .find(|item| item.item_id.0 == "tool_result:call-second")
        .expect("second tool result");
    assert!(second.state.pinned);
    assert!(!second.state.pruned);

    let error = handle
        .evict_context(system.item_id.clone())
        .await
        .expect_err("system policy must not be evictable");
    assert!(
        error
            .to_string()
            .contains("only conversation-resident context items")
    );
    let error = handle
        .pin_context(ContextItemId("tool:inspect".to_owned()))
        .await
        .expect_err("tool definitions must not be mutable through conversation surgery");
    assert!(
        error
            .to_string()
            .contains("only conversation-resident context items")
    );
    assert_eq!(model.request_count(), 0);
}

#[tokio::test]
async fn structured_tool_output_is_toon_only_at_provider_boundary() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(M3Model::new([
        tool_script(&[("call-1", "structured", json!({}))], &[]),
        tool_script(&[("call-2", "structured", json!({}))], &[]),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "structured",
            vec![],
            StubOutcome::Success(ToolResult::new(
                "plain prose",
                json!({"rows": [{"id": 1}, {"id": 2}]}),
            )),
        )))
        .expect("register tool");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                PendingEvent::ToolCallFinished {
                    output: ToolOutput::Mixed { parts },
                    ..
                } if parts.iter().any(|part| matches!(part, ToolOutputPart::Structured { .. }))
            ))
            .count(),
        2
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        let prompt_json = serde_json::to_string(&request.turns).expect("prompt JSON");
        assert_eq!(prompt_json.matches(rw_context::TOON_FORMAT_NOTE).count(), 1);
        assert!(prompt_json.contains("plain prose"));
        assert!(!prompt_json.contains("\"Structured\""));
    }
}

#[tokio::test]
async fn pruning_uses_provider_visible_toon_size_and_persists_that_reclamation() {
    let root = TempDir::new().expect("tempdir");
    let structured_value = json!({
        "rows": (0..30_000)
            .map(|index| json!({"id": index, "state": "candidate-sentinel"}))
            .collect::<Vec<_>>()
    });
    let candidate = Turn {
        role: Role::Tool,
        blocks: vec![Block::ToolResult {
            id: ToolCallId("candidate-call".to_owned()),
            output: ToolOutput::Structured {
                value: structured_value,
            },
            is_error: false,
        }],
        meta: TurnMeta::default(),
    };
    let mut toon = ToonPromptEncoder::default();
    let provider_candidate = prompt_turn(&candidate, &BTreeMap::new(), &mut toon);
    let provider_visible_tokens = LocalTokenEstimator::turn(&provider_candidate);
    let durable_json_tokens = LocalTokenEstimator::turn(&candidate);
    assert!(provider_visible_tokens > 20_000);
    assert_ne!(provider_visible_tokens, durable_json_tokens);

    let assistant_call = |id: &str, name: &str| Turn {
        role: Role::Assistant,
        blocks: vec![Block::ToolCall {
            id: ToolCallId(id.to_owned()),
            name: name.to_owned(),
            args: json!({}),
        }],
        meta: TurnMeta::default(),
    };
    let recent = Turn {
        role: Role::Tool,
        blocks: vec![Block::ToolResult {
            id: ToolCallId("recent-call".to_owned()),
            output: ToolOutput::Text {
                text: "r".repeat(200_000),
            },
            is_error: false,
        }],
        meta: TurnMeta::default(),
    };
    let model = Arc::new(M3Model::new([stop_script("done", &[])]));
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.conversation = vec![
        assistant_call("candidate-call", "shell"),
        candidate,
        text_turn(Role::User, "older user boundary"),
        assistant_call("recent-call", "shell"),
        recent,
        text_turn(Role::User, "newer user boundary"),
    ];
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run pruning").await.expect("message");
    let events = collect_turn(&mut events).await;

    assert!(events.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ToolOutputPruned {
            tool_call_id,
            reclaimed_tokens,
        } if tool_call_id == "candidate-call" && *reclaimed_tokens == provider_visible_tokens
    )));
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let prompt = serde_json::to_string(&requests[0].turns).expect("provider prompt");
    assert!(prompt.contains(PRUNED_TOOL_OUTPUT_REPLACEMENT));
    assert!(!prompt.contains("candidate-sentinel"));
}

#[tokio::test]
async fn stable_prefix_hash_and_hint_remain_identical_across_twenty_turns() {
    let root = TempDir::new().expect("tempdir");
    let mut model = M3Model::new((0..20).map(|_| stop_script("ok", &[])));
    model.metadata.cache_breakpoints = Some(CacheBreakpointSupport::Automatic);
    let model = Arc::new(model);
    let sink = Arc::new(NoopSessionEventSink::default());
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.initial_session_context = vec![text_turn(Role::System, "stable policy")];
    actor_config.event_sink = sink.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut subscription = handle.subscribe().expect("subscription");
    for index in 0..20 {
        handle
            .send_message(format!("message {index}"))
            .await
            .expect("message");
        collect_turn(&mut subscription).await;
    }
    let durable = sink.test_events_after(None).await.expect("durable events");
    let hashes = durable
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ContextUsageUpdated {
                stable_prefix_hash,
                provider_input_tokens: 0,
                ..
            } => Some(stable_prefix_hash.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(hashes.len(), 40);
    assert!(hashes.windows(2).all(|pair| pair[0] == pair[1]));
    let hints = model
        .requests()
        .into_iter()
        .map(|request| request.cache_hint)
        .collect::<Vec<_>>();
    assert!(hints.iter().all(|hint| *hint == hints[0]));
    assert_eq!(hints[0].map(|hint| hint.stable_prefix_turns), Some(1));
}

#[tokio::test]
async fn running_turn_rejects_context_surgery_without_losing_durable_state() {
    let root = TempDir::new().expect("tempdir");
    let mut actor_config = config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.conversation = vec![text_turn(Role::User, "stable item")];
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    handle.ensure_local_driver().await.expect("driver");
    let mut subscription = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    next_matching(&mut subscription, |event| {
        matches!(event, PendingEvent::TurnStarted { .. })
    })
    .await;
    let error = handle
        .pin_context(ContextItemId("conversation:0".to_owned()))
        .await
        .expect_err("running surgery must reject");
    assert!(
        error
            .to_string()
            .contains("context surgery requires an idle session")
    );
    let snapshot = handle
        .context_snapshot()
        .await
        .expect("snapshot remains responsive");
    assert!(!snapshot.items[0].state.pinned);
    assert!(handle.interrupt().await.expect("interrupt"));
    collect_turn(&mut subscription).await;
}
