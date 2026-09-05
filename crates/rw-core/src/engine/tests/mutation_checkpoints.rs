#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::model;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::checkpoints::RecordingCheckpoints;
use crate::engine::tests::fixtures::checkpoints::RecordingFileCheckpoints;
use crate::engine::tests::fixtures::checkpoints::SingleFileCheckpoints;
use crate::engine::tests::fixtures::hooks::MarkPostToolFailed;
use crate::engine::tests::fixtures::hooks::MutatingPreHook;
use crate::engine::tests::fixtures::hooks::SiblingFormatterPostHook;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::descriptor;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::StreamingTool;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use crate::engine::turn;
use futures_util::stream;
use rw_ext::HookEffect;
use rw_ext::HookEvent;
use rw_ext::HookRegistration;
use rw_tools::CapabilityManifest;
use rw_tools::Tool;
use rw_tools::ToolDescriptor;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::Role;
use rw_types::ToolCapability;
use rw_types::ToolOutput;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use tokio::sync::Notify;

#[tokio::test]
async fn mutating_calls_are_sequential_and_checkpointed_before_and_after_each() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[
                ("write-1", "write_fixture", json!({"path": "a"})),
                ("write-2", "write_fixture", json!({"path": "b"})),
            ],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let tool = Arc::new(StubTool::new(
        "write_fixture",
        vec![ToolCapability::WriteFilesystem],
        StubOutcome::Success(ToolResult::new("ok", Value::Null)),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(tool).expect("register tool");
    let checkpoints = Arc::new(RecordingCheckpoints::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.checkpoints = checkpoints.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("write").await.expect("message");
    collect_turn(&mut events).await;
    assert_eq!(
        checkpoints
            .events
            .lock()
            .expect("checkpoint events")
            .as_slice(),
        &[
            "begin:fixture-session:write-1:OpaqueWorkspace",
            "finish:Some(\"write-1\"):Completed",
            "begin:fixture-session:write-2:OpaqueWorkspace",
            "finish:Some(\"write-2\"):Completed",
        ]
    );
}

#[tokio::test]
async fn mutating_post_hook_widens_scope_and_failed_result_finishes_failed_checkpoint() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(&[("read-call", "read_fixture", json!({"path": "a"}))], &[]),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "read_fixture",
            vec![ToolCapability::ReadFilesystem],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        )))
        .expect("register tool");
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new("fixture.mutating-post", HookEvent::PostTool)
                .with_effect(HookEffect::WorkspaceMutating)
                .with_applicable_tools(["read_fixture"]),
            MarkPostToolFailed,
        )
        .expect("post hook");
    let checkpoints = Arc::new(RecordingCheckpoints::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        hooks,
    );
    actor_config.checkpoints = checkpoints.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("read").await.expect("message");
    collect_turn(&mut events).await;
    assert_eq!(
        checkpoints
            .events
            .lock()
            .expect("checkpoint events")
            .as_slice(),
        &[
            "begin:fixture-session:read-call:OpaqueWorkspace",
            "finish:Some(\"read-call\"):Failed",
        ]
    );
}

#[tokio::test]
async fn mutating_formatter_post_hook_sibling_change_is_byte_restored_by_rewind() {
    let root = TempDir::new().expect("tempdir");
    let sibling = root.path().join("formatted.txt");
    let model = Arc::new(ScriptedModel::new([
        stop_script("baseline", &[]),
        tool_script(&[("read-call", "read_fixture", json!({"path": "a"}))], &[]),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "read_fixture",
            vec![ToolCapability::ReadFilesystem],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        )))
        .expect("register tool");
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new("fixture.formatter", HookEvent::PostTool)
                .with_effect(HookEffect::WorkspaceMutating)
                .with_applicable_tools(["read_fixture"]),
            SiblingFormatterPostHook {
                sibling: sibling.clone(),
            },
        )
        .expect("formatter hook");
    let checkpoints = Arc::new(SingleFileCheckpoints {
        path: sibling.clone(),
        snapshots: Mutex::new(Vec::new()),
    });
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        hooks,
    );
    actor_config.checkpoints = checkpoints;
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("baseline").await.expect("baseline");
    collect_turn(&mut events).await;
    handle.send_message("read").await.expect("read");
    collect_turn(&mut events).await;
    assert_eq!(
        std::fs::read_to_string(&sibling).expect("formatted sibling"),
        "formatted sibling"
    );
    handle.send_message("/rewind 1").await.expect("rewind");
    assert!(!sibling.exists());
}

#[tokio::test]
async fn workspace_mutating_pre_hook_runs_only_after_opaque_checkpoint_begin() {
    let root = TempDir::new().expect("tempdir");
    let sibling = root.path().join("sibling.txt");
    let model = Arc::new(ScriptedModel::new([
        stop_script("baseline", &[]),
        tool_script(&[("read-call", "read_fixture", json!({"path": "a"}))], &[]),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "read_fixture",
            vec![ToolCapability::ReadFilesystem],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        )))
        .expect("register tool");
    let ordering = Arc::new(RecordingCheckpoints::default());
    let checkpoints = Arc::new(RecordingFileCheckpoints {
        ordering: Arc::clone(&ordering),
        files: SingleFileCheckpoints {
            path: sibling.clone(),
            snapshots: Mutex::new(Vec::new()),
        },
    });
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new("fixture.mutating-pre", HookEvent::PreTool)
                .with_effect(HookEffect::WorkspaceMutating)
                .with_applicable_tools(["read_fixture"]),
            MutatingPreHook {
                checkpoints: Arc::clone(&ordering),
                sibling: sibling.clone(),
            },
        )
        .expect("pre hook");
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        hooks,
    );
    actor_config.checkpoints = checkpoints.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("baseline").await.expect("baseline");
    collect_turn(&mut events).await;
    handle.send_message("read").await.expect("message");
    collect_turn(&mut events).await;
    assert_eq!(
        std::fs::read_to_string(&sibling).expect("pre-hook sibling"),
        "mutated by pre hook"
    );
    assert_eq!(
        ordering
            .events
            .lock()
            .expect("checkpoint events")
            .as_slice(),
        &[
            "begin:fixture-session:read-call:OpaqueWorkspace",
            "finish:Some(\"read-call\"):Completed",
        ]
    );
    handle.send_message("/rewind 1").await.expect("rewind");
    assert!(!sibling.exists());
}

#[tokio::test]
async fn interrupt_during_mutating_tool_finishes_checkpoint_and_commits_cancelled_result() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([tool_script(
        &[("stream-id", "stream", json!({}))],
        &[],
    )]));
    let release = Arc::new(Notify::new());
    let completed = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StreamingTool {
            descriptor: ToolDescriptor {
                capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
                ..descriptor("stream")
            },
            release,
            completed: completed.clone(),
        }))
        .expect("register tool");
    let checkpoints = Arc::new(RecordingCheckpoints::default());
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.checkpoints = checkpoints.clone();
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::ToolOutput { .. })
    })
    .await;
    assert!(handle.interrupt().await.expect("interrupt"));
    let finished = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { .. })
    })
    .await;
    assert!(matches!(
        finished.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    ));
    assert!(!completed.load(Ordering::SeqCst));
    assert!(
        checkpoints
            .events
            .lock()
            .expect("checkpoint events")
            .iter()
            .any(|event| event.ends_with(":Cancelled"))
    );
    let persisted = sink.events.lock().expect("sink events");
    assert!(persisted.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ConversationTurnCommitted { turn, .. }
            if turn.role == Role::Tool
    )));
}

#[tokio::test]
async fn interrupt_never_starts_later_tools_in_a_sequential_mutating_batch() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([tool_script(
        &[
            ("first", "first_write", json!({})),
            ("second", "second_write", json!({})),
        ],
        &[],
    )]));
    let second = Arc::new(StubTool::new(
        "second_write",
        vec![ToolCapability::WriteFilesystem],
        StubOutcome::Success(ToolResult::new("must not run", Value::Null)),
    ));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StreamingTool {
            descriptor: ToolDescriptor {
                capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
                ..descriptor("first_write")
            },
            release: Arc::new(Notify::new()),
            completed: Arc::new(AtomicBool::new(false)),
        }))
        .expect("first tool");
    tools.register(second.clone()).expect("second tool");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::ToolOutput { .. })
    })
    .await;
    assert!(handle.interrupt().await.expect("interrupt"));
    let turn = collect_turn(&mut events).await;
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        turn.iter()
            .filter(|event| matches!(event.kind, PendingEvent::ToolCallFinished { .. }))
            .count(),
        2
    );
}
