#![cfg(test)]

use crate::engine::pending_event::PendingEvent;
use crate::engine::session;
use crate::engine::tests::fixtures::support::FixedClock;
use crate::engine::tests::fixtures::support::fixture_subagent_result;
use crate::engine::tests::fixtures::tools::ThirdPartyLifecycleTool;
use crate::engine::turn::ActorSubagentEventSink;
use crate::engine::turn::ActorSubagentLifecycleState;
use crate::engine::turn::OrderedSubagentCoordinator;
use crate::engine::turn::TurnSignal;
use rw_tools::SubagentLifecycleEvent;
use rw_tools::SubagentLifecycleMode;
use rw_tools::ToolRegistry;
use rw_types::EventMeta;
use rw_types::PROTOCOL_VERSION;
use rw_types::SequenceId;
use rw_types::SessionId;
use rw_types::SubagentId;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn subagent_lifecycle_launches_in_parallel_and_finishes_in_call_order() {
    let (signals, mut receive) = mpsc::unbounded_channel();
    let coordinator = Arc::new(OrderedSubagentCoordinator::new([0, 1, 2], signals));
    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = Arc::clone(&recorded);
    let actor = tokio::spawn(async move {
        while let Some(signal) = receive.recv().await {
            if let TurnSignal::DurableEvent { kind, respond } = signal {
                let label = match kind {
                    PendingEvent::SubagentSpawned { subagent_id, .. } => {
                        format!("spawn:{}", subagent_id.0)
                    }
                    PendingEvent::SubagentFinished { subagent_id, .. } => {
                        format!("finish:{}", subagent_id.0)
                    }
                    _ => continue,
                };
                captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(label);
                let _ = respond.send(Ok(EventMeta {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: SessionId("fixture".into()),
                    sequence_id: SequenceId(0),
                    emitted_at: FixedClock.emitted_at(),
                    caused_by: None,
                }));
            }
        }
    });
    let sinks = (0..3)
        .map(|index| {
            Arc::new(ActorSubagentEventSink {
                index,
                coordinator: Arc::clone(&coordinator),
                state: Mutex::new(ActorSubagentLifecycleState::default()),
            })
        })
        .collect::<Vec<_>>();
    let spawned = sinks.iter().enumerate().map(|(index, sink)| {
        let sink = Arc::clone(sink);
        async move {
            sink.lifecycle(SubagentLifecycleEvent::Spawned {
                subagent_id: SubagentId(format!("{index}")),
                child_session_id: SessionId(format!("child-{index}")),
                task: format!("task-{index}"),
            })
            .await
        }
    });
    for result in futures_util::future::join_all(spawned).await {
        result.expect("spawn lifecycle");
    }

    let finish_two = {
        let sink = Arc::clone(&sinks[2]);
        tokio::spawn(async move {
            sink.lifecycle(SubagentLifecycleEvent::Finished {
                subagent_id: SubagentId("2".to_owned()),
                result: Box::new(fixture_subagent_result("2")),
            })
            .await
        })
    };
    let finish_one = {
        let sink = Arc::clone(&sinks[1]);
        tokio::spawn(async move {
            sink.lifecycle(SubagentLifecycleEvent::Finished {
                subagent_id: SubagentId("1".to_owned()),
                result: Box::new(fixture_subagent_result("1")),
            })
            .await
        })
    };
    sinks[0]
        .lifecycle(SubagentLifecycleEvent::Finished {
            subagent_id: SubagentId("0".to_owned()),
            result: Box::new(fixture_subagent_result("0")),
        })
        .await
        .expect("finish zero");
    coordinator.advance_after_tool(0);
    finish_one.await.expect("join one").expect("finish one");
    coordinator.advance_after_tool(1);
    finish_two.await.expect("join two").expect("finish two");
    coordinator.advance_after_tool(2);
    drop(sinks);
    drop(coordinator);
    actor.await.expect("actor");
    assert_eq!(
        *recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [
            "spawn:0", "spawn:1", "spawn:2", "finish:0", "finish:1", "finish:2"
        ]
    );
}

#[tokio::test]
async fn failed_spawn_position_is_skipped_without_blocking_later_children() {
    let (signals, mut receive) = mpsc::unbounded_channel();
    let coordinator = Arc::new(OrderedSubagentCoordinator::new([0, 1], signals));
    let actor = tokio::spawn(async move {
        while let Some(signal) = receive.recv().await {
            if let TurnSignal::DurableEvent { respond, .. } = signal {
                let _ = respond.send(Ok(EventMeta {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: SessionId("fixture".into()),
                    sequence_id: SequenceId(0),
                    emitted_at: FixedClock.emitted_at(),
                    caused_by: None,
                }));
            }
        }
    });
    coordinator.advance_after_tool(0);
    let sink = ActorSubagentEventSink {
        index: 1,
        coordinator: Arc::clone(&coordinator),
        state: Mutex::new(ActorSubagentLifecycleState::default()),
    };
    sink.lifecycle(SubagentLifecycleEvent::Spawned {
        subagent_id: SubagentId("valid".to_owned()),
        child_session_id: SessionId("child-valid".to_owned()),
        task: "valid".to_owned(),
    })
    .await
    .expect("later spawn");
    sink.lifecycle(SubagentLifecycleEvent::Finished {
        subagent_id: SubagentId("valid".to_owned()),
        result: Box::new(fixture_subagent_result("valid")),
    })
    .await
    .expect("later finish");
    coordinator.advance_after_tool(1);
    drop(sink);
    drop(coordinator);
    actor.await.expect("actor");
}

#[tokio::test]
async fn third_party_tool_declaration_enables_multiple_lifecycle_producers() {
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(ThirdPartyLifecycleTool))
        .expect("register extension");
    let multi = matches!(
        tools.subagent_lifecycle_mode("third_party_children"),
        Some(SubagentLifecycleMode::MultipleOrdered)
    );
    let (signals, mut receive) = mpsc::unbounded_channel();
    let coordinator = Arc::new(OrderedSubagentCoordinator::new_with_multi(
        [(7, multi)],
        signals,
    ));
    let actor = tokio::spawn(async move {
        let mut count = 0;
        while let Some(signal) = receive.recv().await {
            if let TurnSignal::DurableEvent { respond, .. } = signal {
                count += 1;
                let _ = respond.send(Ok(EventMeta {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: SessionId("fixture".into()),
                    sequence_id: SequenceId(0),
                    emitted_at: FixedClock.emitted_at(),
                    caused_by: None,
                }));
            }
        }
        count
    });
    let sink = ActorSubagentEventSink {
        index: 7,
        coordinator: Arc::clone(&coordinator),
        state: Mutex::new(ActorSubagentLifecycleState::default()),
    };
    for id in ["a", "b"] {
        sink.lifecycle(SubagentLifecycleEvent::Spawned {
            subagent_id: SubagentId(id.to_owned()),
            child_session_id: SessionId(format!("child-{id}")),
            task: id.to_owned(),
        })
        .await
        .expect("spawn");
        sink.lifecycle(SubagentLifecycleEvent::Finished {
            subagent_id: SubagentId(id.to_owned()),
            result: Box::new(fixture_subagent_result(id)),
        })
        .await
        .expect("finish");
    }
    coordinator.advance_after_tool(7);
    drop(sink);
    drop(coordinator);
    assert_eq!(actor.await.expect("actor"), 4);
}

#[tokio::test]
async fn malformed_single_lifecycle_errors_without_hanging_or_persisting_duplicate() {
    let (signals, mut receive) = mpsc::unbounded_channel();
    let coordinator = Arc::new(OrderedSubagentCoordinator::new([7], signals));
    let actor = tokio::spawn(async move {
        let mut count = 0;
        while let Some(signal) = receive.recv().await {
            if let TurnSignal::DurableEvent { respond, .. } = signal {
                count += 1;
                let _ = respond.send(Ok(EventMeta {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: SessionId("fixture".into()),
                    sequence_id: SequenceId(0),
                    emitted_at: FixedClock.emitted_at(),
                    caused_by: None,
                }));
            }
        }
        count
    });
    let sink = ActorSubagentEventSink {
        index: 7,
        coordinator: Arc::clone(&coordinator),
        state: Mutex::new(ActorSubagentLifecycleState::default()),
    };
    sink.lifecycle(SubagentLifecycleEvent::Spawned {
        subagent_id: SubagentId("a".to_owned()),
        child_session_id: SessionId("child-a".to_owned()),
        task: "first".to_owned(),
    })
    .await
    .expect("first spawn");
    let duplicate = timeout(
        Duration::from_millis(100),
        sink.lifecycle(SubagentLifecycleEvent::Spawned {
            subagent_id: SubagentId("b".to_owned()),
            child_session_id: SessionId("child-b".to_owned()),
            task: "duplicate".to_owned(),
        }),
    )
    .await
    .expect("duplicate must not hang")
    .expect_err("duplicate must fail");
    assert!(duplicate.to_string().contains("duplicate active spawn"));
    let mut mismatched = fixture_subagent_result("a");
    mismatched.session_id = SessionId("wrong-session".to_owned());
    assert!(
        sink.lifecycle(SubagentLifecycleEvent::Finished {
            subagent_id: SubagentId("a".to_owned()),
            result: Box::new(mismatched),
        })
        .await
        .expect_err("mismatched finish must fail without consuming active spawn")
        .to_string()
        .contains("identity does not match")
    );
    sink.lifecycle(SubagentLifecycleEvent::Finished {
        subagent_id: SubagentId("a".to_owned()),
        result: Box::new(fixture_subagent_result("a")),
    })
    .await
    .expect("matching finish");
    assert!(
        sink.lifecycle(SubagentLifecycleEvent::Finished {
            subagent_id: SubagentId("a".to_owned()),
            result: Box::new(fixture_subagent_result("a")),
        })
        .await
        .is_err()
    );
    coordinator.advance_after_tool(7);
    drop(sink);
    drop(coordinator);
    assert_eq!(actor.await.expect("actor"), 2);
}
