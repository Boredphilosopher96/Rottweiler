#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS;
use crate::engine::MAX_LIVE_TOOL_OUTPUT_CHUNKS;
use crate::engine::MAX_TOOL_EXECUTION_WINDOW;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::model;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::descriptor;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::EmptySequentialTool;
use crate::engine::tests::fixtures::tools::FloodOutputTool;
use crate::engine::tests::fixtures::tools::OrderedWindowProbe;
use crate::engine::tests::fixtures::tools::ReverseCompletionTool;
use crate::engine::tests::fixtures::tools::SaturatingOrderedTool;
use crate::engine::tests::fixtures::tools::SessionCaptureTool;
use crate::engine::tests::fixtures::tools::StreamingTool;
use crate::engine::turn;
use futures_util::stream;
use rw_tools::CapabilityManifest;
use rw_tools::ToolDescriptor;
use rw_tools::ToolRegistry;
use rw_types::SessionId;
use rw_types::ToolCapability;
use rw_types::ToolOutput;
use rw_types::config::PermissionDecision;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::time::timeout;

#[tokio::test]
async fn parallel_tools_finish_reverse_but_emit_results_in_call_index_order() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[
                ("first-id", "first", json!({})),
                ("second-id", "second", json!({})),
            ],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let release_first = Arc::new(Notify::new());
    let completion_order = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    for (name, first) in [("first", true), ("second", false)] {
        tools
            .register(Arc::new(ReverseCompletionTool {
                descriptor: descriptor(name),
                first,
                release_first: release_first.clone(),
                completion_order: completion_order.clone(),
            }))
            .expect("register tool");
    }
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(
        completion_order
            .lock()
            .expect("completion order")
            .as_slice(),
        &["second", "first"]
    );
    let indices = events
        .iter()
        .filter_map(|event| match event.kind {
            PendingEvent::ToolCallFinished { index, .. } => Some(index),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(indices, vec![0, 1]);
}

#[tokio::test]
async fn ordered_tool_window_bounds_completed_tail_and_cancels_unstarted_calls() {
    let root = TempDir::new().expect("tempdir");
    let count = MAX_TOOL_EXECUTION_WINDOW * 3;
    let calls = (0..count)
        .map(|index| (format!("call-{index}"), json!({"index": index})))
        .collect::<Vec<_>>();
    let script_calls = calls
        .iter()
        .map(|(id, args)| (id.as_str(), "window_probe", args.clone()))
        .collect::<Vec<_>>();
    let model = Arc::new(ScriptedModel::new([tool_script(&script_calls, &[])]));
    let probe = Arc::new(OrderedWindowProbe {
        started: AtomicUsize::new(0),
        window_filled: Notify::new(),
        exceeded_window: Notify::new(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(probe.clone()).expect("probe tool");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("fill window").await.expect("message");
    tokio::time::timeout(Duration::from_secs(2), probe.window_filled.notified())
        .await
        .expect("first execution window started");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), probe.exceeded_window.notified())
            .await
            .is_err(),
        "completed later results must remain charged to the ordered window"
    );
    handle.interrupt().await.expect("interrupt");
    let events = collect_turn(&mut events).await;
    assert_eq!(
        probe.started.load(Ordering::SeqCst),
        MAX_TOOL_EXECUTION_WINDOW
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| { matches!(event.kind, PendingEvent::ToolCallFinished { .. }) })
            .count(),
        count
    );
}

#[tokio::test]
async fn mixed_tools_parallelize_read_runs_between_mutation_barriers() {
    let root = TempDir::new().expect("tempdir");
    let names = ["read_one", "read_two", "write", "read_three", "read_four"];
    let calls = names
        .iter()
        .enumerate()
        .map(|(index, name)| (format!("call-{index}"), *name, json!({})))
        .collect::<Vec<_>>();
    let script_calls = calls
        .iter()
        .map(|(id, name, args)| (id.as_str(), *name, args.clone()))
        .collect::<Vec<_>>();
    let model = Arc::new(ScriptedModel::new([
        tool_script(&script_calls, &[]),
        stop_script("done", &[]),
    ]));
    let completion_order = Arc::new(Mutex::new(Vec::new()));
    let first_run = Arc::new(Notify::new());
    let last_run = Arc::new(Notify::new());
    let mut tools = ToolRegistry::new();
    for (index, name) in names.into_iter().enumerate() {
        let mut tool_descriptor = descriptor(name);
        if index == 2 {
            tool_descriptor.capabilities =
                CapabilityManifest::new([ToolCapability::WriteFilesystem]);
        }
        tools
            .register(Arc::new(ReverseCompletionTool {
                descriptor: tool_descriptor,
                first: index == 0 || index == 3,
                release_first: if index < 3 {
                    first_run.clone()
                } else {
                    last_run.clone()
                },
                completion_order: completion_order.clone(),
            }))
            .expect("register mixed tool");
    }
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("run mixed tools")
        .await
        .expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(
        completion_order
            .lock()
            .expect("completion order")
            .as_slice(),
        &["read_two", "read_one", "write", "read_four", "read_three"],
    );
    let indices = events
        .iter()
        .filter_map(|event| match event.kind {
            PendingEvent::ToolCallFinished { index, .. } => Some(index),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(indices, vec![0, 1, 2, 3, 4]);
}

#[tokio::test]
async fn parallel_tool_output_makes_progress_when_later_tool_saturates_buffer() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[
                ("first-id", "delayed_first", json!({})),
                ("second-id", "flood_later", json!({})),
                ("third-id", "flood_later", json!({})),
            ],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let background_full = Arc::new(Notify::new());
    let mut tools = ToolRegistry::new();
    for first in [true, false] {
        tools
            .register(Arc::new(SaturatingOrderedTool {
                first,
                background_full: Arc::clone(&background_full),
            }))
            .expect("register tool");
    }
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("stream both tools")
        .await
        .expect("message");
    let completed = timeout(
        Duration::from_secs(2),
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { .. })
        }),
    )
    .await;
    if completed.is_err() {
        handle.interrupt().await.expect("cancel stalled regression");
    }
    let finished = completed.expect("later output cannot block the current tool");
    assert!(matches!(
        finished.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        }
    ));
    let recorded = sink.events.lock().expect("events");
    let outputs = recorded
        .iter()
        .filter_map(|event| match &event.kind {
            PendingEvent::ToolOutput { id, chunk, .. } => Some((id.as_str(), chunk.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 1 + MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS * 4);
    assert_eq!(outputs[0], ("first-id", "delayed_first:0"));
    for (expected_id, chunks) in ["second-id", "third-id"]
        .into_iter()
        .zip(outputs[1..].chunks_exact(MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS * 2))
    {
        for (index, (id, chunk)) in chunks.iter().enumerate() {
            assert_eq!(*id, expected_id);
            assert_eq!(*chunk, format!("flood_later:{index}"));
        }
    }
}

#[tokio::test]
async fn empty_manifest_stateful_calls_never_run_in_parallel() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[
                ("first-id", "stateful_first", json!({})),
                ("second-id", "stateful_second", json!({})),
            ],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let second_started = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    for (name, first) in [("stateful_first", true), ("stateful_second", false)] {
        tools
            .register(Arc::new(EmptySequentialTool {
                descriptor: ToolDescriptor {
                    name: name.to_owned(),
                    description: "stateful fixture".to_owned(),
                    input_schema: json!({"type": "object"}),
                    capabilities: CapabilityManifest::default(),
                },
                first,
                first_started: first_started.clone(),
                release_first: release_first.clone(),
                second_started: second_started.clone(),
            }))
            .expect("register tool");
    }
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    timeout(Duration::from_secs(3), first_started.notified())
        .await
        .expect("first tool started");
    assert!(!second_started.load(Ordering::SeqCst));
    release_first.notify_one();
    collect_turn(&mut events).await;
    assert!(second_started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn tool_contexts_keep_stateful_data_isolated_by_session_id() {
    let root = TempDir::new().expect("tempdir");
    let sessions = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(SessionCaptureTool {
        sessions: sessions.clone(),
    });
    for session_id in ["session-a", "session-b"] {
        let model = Arc::new(ScriptedModel::new([
            tool_script(&[("capture", "session_capture", json!({}))], &[]),
            stop_script("done", &[]),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(tool.clone()).expect("register tool");
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.session_id = SessionId(session_id.to_owned());
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe().expect("subscription");
        handle.send_message("capture").await.expect("message");
        collect_turn(&mut events).await;
    }
    assert_eq!(
        sessions.lock().expect("captured sessions").as_slice(),
        &["session-a", "session-b"]
    );
}

#[tokio::test]
async fn earliest_running_tool_streams_before_it_completes() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(&[("stream-id", "stream", json!({}))], &[]),
        stop_script("done", &[]),
    ]));
    let release = Arc::new(Notify::new());
    let completed = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StreamingTool {
            descriptor: descriptor("stream"),
            release: release.clone(),
            completed: completed.clone(),
        }))
        .expect("register tool");
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
    let chunk = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::ToolOutput { .. })
    })
    .await;
    assert!(matches!(
        chunk.kind,
        PendingEvent::ToolOutput { chunk, .. } if chunk == "live chunk"
    ));
    assert!(!completed.load(Ordering::SeqCst));
    release.notify_one();
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { .. })
    })
    .await;
    assert!(completed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn bounded_live_output_drains_excess_chunks_and_finishes() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(&[("flood-call", "flood", json!({}))], &[]),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(FloodOutputTool))
        .expect("flood tool");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("flood").await.expect("message");
    let turn = timeout(Duration::from_secs(3), collect_turn(&mut events))
        .await
        .expect("flood turn must not hang");
    let chunks = turn
        .iter()
        .filter_map(|event| match &event.kind {
            PendingEvent::ToolOutput { chunk, .. } => Some(chunk),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(chunks.len() <= MAX_LIVE_TOOL_OUTPUT_CHUNKS.saturating_add(1));
    assert!(chunks.iter().any(|chunk| chunk.contains("truncated")));
}
