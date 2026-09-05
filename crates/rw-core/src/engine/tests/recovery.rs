#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::SessionUsage;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::model;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::SessionRecoveredState;
use crate::engine::projection::project_session_events;
use crate::engine::recovery;
use crate::engine::session;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::wire_event;
use crate::engine::turn;
use crate::engine::unavailable_cost;
use futures_util::StreamExt;
use rw_tools::Tool;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::Block;
use rw_types::Role;
use rw_types::ToolCallId;
use rw_types::ToolOutput;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::config::PermissionDecision;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[test]
fn projector_preserves_committed_partial_ir_and_marks_kill_tail_interrupted() {
    let user = Turn {
        role: Role::User,
        blocks: vec![Block::Text {
            text: "inspect".to_owned(),
        }],
        meta: TurnMeta::default(),
    };
    let partial = Turn {
        role: Role::Assistant,
        blocks: vec![
            Block::Thinking {
                content: "opaque".to_owned(),
                signature: Some("signed".to_owned()),
            },
            Block::Text {
                text: "partial".to_owned(),
            },
            Block::Citation {
                uri: "https://example.invalid/source".to_owned(),
                title: Some("source".to_owned()),
                excerpt: None,
            },
        ],
        meta: TurnMeta::default(),
    };
    let events = vec![
        wire_event(0, PendingEvent::TurnStarted { turn: 1 }),
        wire_event(
            1,
            PendingEvent::UserMessageAccepted {
                turn: 1,
                content: "inspect".to_owned(),
                attachments: Vec::new(),
            },
        ),
        wire_event(
            2,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: user.clone(),
            },
        ),
        wire_event(
            3,
            PendingEvent::ThinkingDelta {
                turn: 1,
                content: "opaque".to_owned(),
                signature: Some("signed".to_owned()),
            },
        ),
        wire_event(
            4,
            PendingEvent::TextDelta {
                turn: 1,
                text: "partial".to_owned(),
            },
        ),
        wire_event(
            5,
            PendingEvent::CitationDelta {
                turn: 1,
                uri: "https://example.invalid/source".to_owned(),
                title: Some("source".to_owned()),
            },
        ),
    ];
    let recovered = project_session_events(&events).expect("project events");
    assert_eq!(recovered.conversation, vec![user, partial]);
    assert_eq!(recovered.interrupted_turn, Some(1));
    assert_eq!(recovered.next_turn, 2);
    assert_eq!(recovered.last_sequence, Some(5.into()));
}

#[test]
fn projector_rewind_clears_future_queue_failed_uncommitted_and_partial_state() {
    let committed_user = Turn {
        role: Role::User,
        blocks: vec![Block::Text {
            text: "kept user".to_owned(),
        }],
        meta: TurnMeta::default(),
    };
    let committed_assistant = Turn {
        role: Role::Assistant,
        blocks: vec![Block::Text {
            text: "kept answer".to_owned(),
        }],
        meta: TurnMeta::default(),
    };
    let kinds = vec![
        PendingEvent::TurnStarted { turn: 1 },
        PendingEvent::UserMessageAccepted {
            turn: 1,
            content: "kept user".to_owned(),
            attachments: Vec::new(),
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: committed_user.clone(),
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: committed_assistant.clone(),
        },
        PendingEvent::TurnFinished {
            turn: 1,
            status: AgentTurnStatus::Completed,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
        },
        PendingEvent::MessageQueued {
            position: 1,
            content: "future duplicate".to_owned(),
            attachments: Vec::new(),
        },
        PendingEvent::TurnStarted { turn: 2 },
        PendingEvent::UserMessageAccepted {
            turn: 2,
            content: "future duplicate".to_owned(),
            attachments: Vec::new(),
        },
        PendingEvent::TextDelta {
            turn: 2,
            text: "future partial".to_owned(),
        },
        PendingEvent::TurnFinished {
            turn: 2,
            status: AgentTurnStatus::Failed,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
        },
        PendingEvent::MessageQueued {
            position: 1,
            content: "queued after failure".to_owned(),
            attachments: Vec::new(),
        },
        PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind-fixture".to_owned(),
            unrestorable_paths: Vec::new(),
        },
    ];
    let events = kinds
        .into_iter()
        .enumerate()
        .map(|(sequence, kind)| {
            wire_event(u64::try_from(sequence).expect("fixture sequence"), kind)
        })
        .collect::<Vec<_>>();
    let recovered = project_session_events(&events).expect("project rewind");
    assert_eq!(
        recovered.conversation,
        vec![committed_user, committed_assistant]
    );
    assert!(recovered.queued_messages.is_empty());
    assert_eq!(recovered.interrupted_turn, None);
    assert_eq!(recovered.turn_ends, BTreeMap::from([(1, 2)]));
    assert_eq!(recovered.completed_turns, 1);
}

#[test]
fn projector_kill_boundaries_never_duplicate_committed_tool_calls_or_results() {
    let user = Turn {
        role: Role::User,
        blocks: vec![Block::Text {
            text: "use tool".to_owned(),
        }],
        meta: TurnMeta::default(),
    };
    let assistant = Turn {
        role: Role::Assistant,
        blocks: vec![Block::ToolCall {
            id: ToolCallId("call".to_owned()),
            name: "fixture".to_owned(),
            args: json!({}),
        }],
        meta: TurnMeta::default(),
    };
    let tool = Turn {
        role: Role::Tool,
        blocks: vec![Block::ToolResult {
            id: ToolCallId("call".to_owned()),
            output: ToolOutput::Text {
                text: "done".to_owned(),
            },
            is_error: false,
        }],
        meta: TurnMeta::default(),
    };
    let kinds = vec![
        PendingEvent::TurnStarted { turn: 1 },
        PendingEvent::UserMessageAccepted {
            turn: 1,
            content: "use tool".to_owned(),
            attachments: Vec::new(),
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: user,
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: assistant,
        },
        PendingEvent::ToolCallStarted {
            invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
            turn: 1,
            id: "call".to_owned(),
            name: "fixture".to_owned(),
            arguments: json!({}),
            index: 0,
        },
        PendingEvent::ToolCallFinished {
            invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
            turn: 1,
            id: "call".to_owned(),
            output: ToolOutput::Text {
                text: "done".to_owned(),
            },
            is_error: false,
            index: 0,
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: tool,
        },
    ];
    let events = kinds
        .into_iter()
        .enumerate()
        .map(|(sequence, kind)| {
            wire_event(u64::try_from(sequence).expect("fixture sequence"), kind)
        })
        .collect::<Vec<_>>();
    for (length, expected_results) in [(4, 1), (5, 1), (6, 1), (7, 1)] {
        let recovered = project_session_events(&events[..length]).expect("project prefix");
        let calls = recovered
            .conversation
            .iter()
            .flat_map(|turn| &turn.blocks)
            .filter(|block| matches!(block, Block::ToolCall { .. }))
            .count();
        let results = recovered
            .conversation
            .iter()
            .flat_map(|turn| &turn.blocks)
            .filter(|block| matches!(block, Block::ToolResult { .. }))
            .count();
        assert_eq!(calls, 1, "prefix {length}");
        assert_eq!(results, expected_results, "prefix {length}");
    }
}

#[tokio::test]
async fn resume_durably_closes_projected_inflight_turn_before_new_commands() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
        batch_sizes: Mutex::new(Vec::new()),
        tail_floor: Mutex::new(Some(5.into())),
    });
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    actor_config.recovered = SessionRecoveredState {
        conversation: vec![Turn {
            role: Role::Assistant,
            blocks: vec![Block::Text {
                text: "partial".to_owned(),
            }],
            meta: TurnMeta::default(),
        }],
        queued_messages: Vec::new(),
        completed_turns: 0,
        next_turn: 2,
        last_sequence: Some(5.into()),
        interrupted_turn: Some(1),
        turn_ends: BTreeMap::new(),
        ..SessionRecoveredState::default()
    };
    let handle = SessionActor::spawn(actor_config).expect("actor");
    handle.send_message("/status").await.expect("status");
    let persisted = sink.events.lock().expect("sink events");
    assert!(matches!(
        persisted.first().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            turn: 1,
            status: AgentTurnStatus::Interrupted,
            ..
        })
    ));
    assert_eq!(persisted[0].sequence, 6.into());
}

#[tokio::test]
async fn resume_closes_interrupted_tail_then_auto_starts_recovered_queue() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([stop_script("queue resumed", &[])]));
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
        batch_sizes: Mutex::new(Vec::new()),
        tail_floor: Mutex::new(Some(9.into())),
    });
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    actor_config.recovered = SessionRecoveredState {
        conversation: vec![Turn {
            role: Role::Assistant,
            blocks: vec![Block::Text {
                text: "partial prior answer".to_owned(),
            }],
            meta: TurnMeta::default(),
        }],
        queued_messages: vec!["queued during crash".to_owned()],
        completed_turns: 0,
        next_turn: 2,
        last_sequence: Some(9.into()),
        interrupted_turn: Some(1),
        turn_ends: BTreeMap::new(),
        ..SessionRecoveredState::default()
    };
    let handle = SessionActor::spawn(actor_config).expect("actor");
    timeout(Duration::from_secs(3), async {
        loop {
            if handle.snapshot().await.expect("snapshot").completed_turns == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("recovered queue completion");
    assert_eq!(model.request_count(), 1);
    let persisted = sink.events.lock().expect("sink events");
    let kinds = persisted
        .iter()
        .map(|event| &event.kind)
        .collect::<Vec<_>>();
    assert!(matches!(
        kinds.first(),
        Some(PendingEvent::TurnFinished {
            turn: 1,
            status: AgentTurnStatus::Interrupted,
            ..
        })
    ));
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, PendingEvent::TurnStarted { turn: 2 }))
    );
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        PendingEvent::UserMessageAccepted { turn: 2, content, .. }
            if content == "queued during crash"
    )));
}

#[test]
fn interrupted_repair_ids_remain_stable_when_recovery_itself_crashes() {
    let mut events = vec![
        wire_event(0, PendingEvent::TurnStarted { turn: 1 }),
        wire_event(
            1,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: Turn {
                    role: Role::Assistant,
                    blocks: ["first", "second"]
                        .into_iter()
                        .map(|id| Block::ToolCall {
                            id: ToolCallId(id.to_owned()),
                            name: "fixture".to_owned(),
                            args: json!({}),
                        })
                        .collect(),
                    meta: TurnMeta::default(),
                },
            },
        ),
    ];
    let first = project_session_events(&events).expect("first recovery");
    assert_eq!(first.interrupted_tool_repairs.len(), 2);
    let expected = first.interrupted_tool_repairs[1].invocation_id.clone();
    for kind in session::interrupted_tool_recovery_events(&first.interrupted_tool_repairs[0]) {
        events.push(wire_event(
            u64::try_from(events.len()).expect("sequence"),
            kind,
        ));
    }
    let resumed = project_session_events(&events).expect("recovery after partial repair");
    assert_eq!(resumed.interrupted_tool_repairs.len(), 1);
    assert_eq!(resumed.interrupted_tool_repairs[0].tool_call_id.0, "second");
    assert_eq!(resumed.interrupted_tool_repairs[0].invocation_id, expected);
    assert_ne!(
        resumed.interrupted_tool_repairs[0].invocation_id,
        first.interrupted_tool_repairs[0].invocation_id
    );
}

#[test]
fn interrupted_reused_provider_id_finishes_only_its_active_invocation() {
    let mut events = vec![PendingEvent::TurnStarted { turn: 1 }];
    for iteration in 0..2 {
        let invocation_id = rw_types::ToolInvocationId(format!("invocation-{iteration}"));
        events.push(PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: Turn {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall {
                    id: ToolCallId("reused".to_owned()),
                    name: "fixture".to_owned(),
                    args: json!({"iteration":iteration}),
                }],
                meta: TurnMeta::default(),
            },
        });
        events.push(PendingEvent::ToolCallStarted {
            turn: 1,
            id: "reused".to_owned(),
            invocation_id: invocation_id.clone(),
            name: "fixture".to_owned(),
            arguments: json!({"iteration":iteration}),
            index: 0,
        });
        if iteration == 0 {
            let output = ToolOutput::Text {
                text: "first result".to_owned(),
            };
            events.push(PendingEvent::ToolCallFinished {
                turn: 1,
                id: "reused".to_owned(),
                invocation_id,
                output: output.clone(),
                is_error: false,
                index: 0,
            });
            events.push(PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: Turn {
                    role: Role::Tool,
                    blocks: vec![Block::ToolResult {
                        id: ToolCallId("reused".to_owned()),
                        output,
                        is_error: false,
                    }],
                    meta: TurnMeta::default(),
                },
            });
        }
    }
    let events = events
        .into_iter()
        .enumerate()
        .map(|(index, event)| wire_event(u64::try_from(index).expect("index"), event))
        .collect::<Vec<_>>();
    let recovered = project_session_events(&events).expect("recovery");
    assert_eq!(recovered.interrupted_tool_repairs.len(), 1);
    let repair = &recovered.interrupted_tool_repairs[0];
    assert_eq!(repair.invocation_id.0, "invocation-1");
    assert!(repair.missing_start.is_none());
    let repair_events = session::interrupted_tool_recovery_events(repair);
    assert_eq!(repair_events.len(), 1);
    assert!(
        matches!(&repair_events[0], PendingEvent::ToolCallFinished { invocation_id, .. } if invocation_id.0 == "invocation-1")
    );
}

#[tokio::test]
async fn resume_persists_tool_result_repairs_before_interrupted_closure() {
    let root = TempDir::new().expect("tempdir");
    let original = vec![
        wire_event(0, PendingEvent::TurnStarted { turn: 1 }),
        wire_event(
            1,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: Turn {
                    role: Role::Assistant,
                    blocks: vec![Block::ToolCall {
                        id: ToolCallId("lost-call".to_owned()),
                        name: "fixture".to_owned(),
                        args: json!({}),
                    }],
                    meta: TurnMeta::default(),
                },
            },
        ),
    ];
    let recovered = project_session_events(&original).expect("project kill tail");
    assert_eq!(recovered.interrupted_tool_repairs.len(), 1);
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
        batch_sizes: Mutex::new(Vec::new()),
        tail_floor: Mutex::new(Some(1.into())),
    });
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered = recovered;
    actor_config.event_sink = sink.clone();
    let _handle = SessionActor::spawn(actor_config).expect("actor");
    timeout(Duration::from_secs(1), async {
        loop {
            if sink.events.lock().expect("events").len() >= 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable recovery closure");
    let repairs = sink.events.lock().expect("events").clone();
    assert!(
        matches!(&repairs[0].kind, PendingEvent::ToolCallStarted { id, invocation_id, name, .. }
            if id == "lost-call" && invocation_id.0 == "turn-1:repair-0" && name == "fixture")
    );
    assert!(matches!(
        repairs[1].kind,
        PendingEvent::ToolCallFinished {
            ref id,
            is_error: true,
            ..
        } if id == "lost-call"
    ));
    assert!(matches!(
        repairs[2].kind,
        PendingEvent::ConversationTurnCommitted {
            turn: Turn {
                role: Role::Tool,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        repairs[3].kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    ));
    let mut durable = original;
    durable.extend(repairs.into_iter().map(|event| event.wire));
    let projected = project_session_events(&durable).expect("project repaired log");
    assert_eq!(projected.interrupted_turn, None);
    assert_eq!(
        projected
            .conversation
            .iter()
            .flat_map(|turn| &turn.blocks)
            .filter(|block| matches!(block, Block::ToolResult { .. }))
            .count(),
        1
    );
}
