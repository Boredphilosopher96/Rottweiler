#![allow(clippy::expect_used)]
use super::*;
use crate::server::ServerEngine as _;
use rw_store::session::SessionEventLog;
use rw_types::session_read::{SessionReadAncestor, SessionReadScope};
use rw_types::{
    Block, ClientRole, CommandMeta, CommandReply, EventMeta, RequestId, Role, Turn, TurnMeta,
    transcript::{TranscriptPosition, TranscriptRead, TranscriptReadResult},
};

fn meta(request: &str) -> CommandMeta {
    CommandMeta {
        protocol_version: rw_core::PROTOCOL_VERSION,
        client_id: ClientId("spoofed".into()),
        request_id: RequestId(request.into()),
    }
}
fn fixture(count: u64) -> (tempfile::TempDir, HistoricalReplayEngine) {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SessionEventLog::open(root.path(), "history").expect("journal");
    for sequence in 0..count {
        journal
            .append(EngineEvent::ConversationTurnCommitted {
                meta: EventMeta {
                    protocol_version: rw_core::PROTOCOL_VERSION,
                    session_id: SessionId("history".into()),
                    sequence_id: SequenceId(sequence),
                    emitted_at: "2026-09-04T00:00:00Z".into(),
                    caused_by: None,
                },
                agent_turn: sequence,
                turn: Turn {
                    role: Role::User,
                    blocks: vec![Block::Text {
                        text: format!("body-{sequence}"),
                    }],
                    meta: TurnMeta::default(),
                },
            })
            .expect("append");
    }
    drop(journal);
    let engine =
        HistoricalReplayEngine::open(root.path(), SessionId("history".into())).expect("engine");
    (root, engine)
}
fn read(id: &str) -> ClientCommand {
    ClientCommand::ReadTranscript {
        scope: rw_types::session_read::SessionReadScope::Session {},
        meta: meta(id),
        session_id: SessionId("history".into()),
        read: TranscriptRead {
            known_view: None,
            position: TranscriptPosition::Latest {},
            max_items: 4,
            max_bytes: 64 * 1024,
        },
    }
}

#[tokio::test]
async fn historical_availability_is_not_raw_replay_and_pages_are_direct_authenticated_reads() {
    let (_root, engine) = fixture(300);
    let client = ClientId("bound".into());
    let mut events = engine
        .subscribe(client.clone(), Some(SessionId("history".into())), None)
        .await
        .expect("subscribe");
    let event = events
        .recv()
        .await
        .expect("event")
        .expect("history response");
    let event: EngineEvent = serde_json::from_slice(&event.json).expect("protocol JSON");
    assert!(matches!(
        event,
        EngineEvent::SessionHistoryReady {
            through_sequence: Some(SequenceId(299)),
            ..
        }
    ));
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let mut replies = Vec::new();
    for id in ["catchup", "page"] {
        let reply = engine
            .dispatch(client.clone(), read(id))
            .await
            .expect("reply");
        replies.push(serde_json::from_slice::<CommandReply>(&reply.bytes).expect("typed reply"));
    }
    assert!(matches!(&replies[0], CommandReply::Read { events, .. }
        if matches!(&events[..], [EngineEvent::TranscriptPageReady {
            result: TranscriptReadResult::CatchingUp { .. }, .. }])));
    let CommandReply::Read {
        outcome: CommandOutcome::Accepted {},
        events: page_events,
    } = &replies[1]
    else {
        panic!("read page")
    };
    let [
        EngineEvent::TranscriptPageReady {
            meta,
            result: TranscriptReadResult::Ready { page },
            ..
        },
    ] = &page_events[..]
    else {
        panic!("page result")
    };
    assert_eq!(meta.client_id, client);
    assert_eq!(meta.request_id.0, "page");
    assert_eq!(page.items.len(), 4);
    assert_eq!(page.items[0].ordinal.0, 296);
    assert_eq!(page.items[3].id.0, SequenceId(299));
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "historical reads never enter the retained SSE stream"
    );
    assert!(
        engine
            .subscribe(
                client,
                Some(SessionId("history".into())),
                Some(SequenceId(300))
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn historical_attachment_cannot_gain_driver_or_mutation_capability() {
    let (_root, engine) = fixture(1);
    for (role, accepted) in [(ClientRole::Observer, true), (ClientRole::Driver, false)] {
        let reply = engine
            .dispatch(
                ClientId("bound".into()),
                ClientCommand::AttachSession {
                    meta: meta("attach"),
                    session_id: SessionId("history".into()),
                    last_seen_sequence: None,
                    role,
                },
            )
            .await
            .expect("attach reply");
        assert_eq!(reply.outcome == CommandOutcome::Accepted {}, accepted);
    }
    let reply = engine
        .dispatch(
            ClientId("bound".into()),
            ClientCommand::Interrupt {
                meta: meta("interrupt"),
                session_id: SessionId("history".into()),
            },
        )
        .await
        .expect("reply");
    assert!(matches!(reply.outcome, CommandOutcome::Rejected { .. }));
    assert!(
        engine
            .subscribe(
                ClientId("bound".into()),
                Some(SessionId("foreign".into())),
                None
            )
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn historical_process_gets_read_only_runtime_and_leaves_no_build_junk() {
    use std::{fs, os::unix::fs::PermissionsExt as _};
    let (storage, _engine) = fixture(1);
    let fixture = storage.path().join("fixture-tui");
    fs::write(storage.path().join("keybindings.toml"), "preset = 'vim'").expect("keybindings");
    fs::write(
        &fixture,
        b"#!/bin/sh\n\
        test \"$ROTTWEILER_REPLAY_MODE\" = \"1\" || exit 11\n\
        test \"$ROTTWEILER_SESSION_ID\" = \"history\" || exit 12\n\
        test -S \"$ROTTWEILER_ENGINE_SOCKET\" || exit 13\n\
        test -f \"$ROTTWEILER_ENGINE_TOKEN_FILE\" || exit 14\n\
        test \"$ROTTWEILER_TUI_KEYBINDINGS\" = \"preset = 'vim'\" || exit 15\n",
    )
    .expect("fixture script");
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700)).expect("permissions");
    run_history_replay_with_tui(storage.path(), "history", &fixture)
        .await
        .expect("process");
    assert!(
        fs::read_dir(storage.path().join("run"))
            .expect("runtime root")
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn historical_task_reads_are_source_owned_and_session_bound() {
    let (root, engine) = fixture(1);
    let snapshot = rw_types::todo::TodoSnapshot {
        items: vec![rw_types::todo::TodoItem {
            id: "audit".into(),
            content: "Inspect the source-owned task state".into(),
            status: rw_types::todo::TodoStatus::Pending,
        }],
    };
    let mut journal = SessionEventLog::open(root.path(), "history").expect("journal");
    journal
        .append(EngineEvent::TodoStateCommitted {
            meta: EventMeta {
                protocol_version: rw_core::PROTOCOL_VERSION,
                session_id: SessionId("history".into()),
                sequence_id: SequenceId(1),
                emitted_at: "2026-09-04T00:00:00Z".into(),
                caused_by: None,
            },
            snapshot: snapshot.clone(),
        })
        .expect("commit task state");
    drop(journal);
    let reply = engine
        .dispatch(
            ClientId("bound".into()),
            ClientCommand::GetTodos {
                scope: rw_types::session_read::SessionReadScope::Session {},
                meta: meta("tasks"),
                session_id: SessionId("history".into()),
            },
        )
        .await
        .expect("task reply");
    let parsed: CommandReply = serde_json::from_slice(&reply.bytes).expect("typed reply");
    assert!(
        matches!(parsed, CommandReply::Read { outcome: CommandOutcome::Accepted {}, events }
        if matches!(&events[..], [EngineEvent::TodosRead { result: rw_types::todo::TodoReadResult::Ready { todos }, .. }]
            if todos.through == Some(SequenceId(1)) && todos.snapshot == snapshot))
    );
    let foreign = engine
        .dispatch(
            ClientId("bound".into()),
            ClientCommand::GetTodos {
                scope: rw_types::session_read::SessionReadScope::Session {},
                meta: meta("foreign-tasks"),
                session_id: SessionId("foreign".into()),
            },
        )
        .await
        .expect("rejected reply");
    let parsed: CommandReply = serde_json::from_slice(&foreign.bytes).expect("typed rejection");
    assert!(
        matches!(parsed, CommandReply::Read { outcome: CommandOutcome::Rejected { .. }, events } if events.is_empty())
    );
}

fn descendant_meta(session: &str, sequence: u64) -> EventMeta {
    EventMeta {
        protocol_version: rw_core::PROTOCOL_VERSION,
        session_id: SessionId(session.into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-05T00:00:00Z".into(),
        caused_by: None,
    }
}

fn descendant_fixture() -> (tempfile::TempDir, HistoricalReplayEngine, SessionReadScope) {
    let (root, engine) = fixture(1);
    let mut parent = SessionEventLog::open(root.path(), "history").expect("parent");
    parent
        .append(EngineEvent::TurnStarted {
            meta: descendant_meta("history", 1),
            turn_id: rw_types::TurnId("1".into()),
        })
        .expect("turn");
    parent
        .append(EngineEvent::SubagentSpawned {
            meta: descendant_meta("history", 2),
            subagent_id: rw_types::SubagentId("agent".into()),
            child_session_id: SessionId("child".into()),
            task: "Inspect".into(),
        })
        .expect("spawn");
    drop(parent);
    let mut child = SessionEventLog::open(root.path(), "child").expect("child");
    child
        .append(EngineEvent::ConversationTurnCommitted {
            meta: descendant_meta("child", 0),
            agent_turn: 0,
            turn: Turn {
                role: Role::User,
                blocks: vec![Block::Text {
                    text: "child body".into(),
                }],
                meta: TurnMeta::default(),
            },
        })
        .expect("child message");
    child
        .append(EngineEvent::TodoStateCommitted {
            meta: descendant_meta("child", 1),
            snapshot: rw_types::todo::TodoSnapshot::default(),
        })
        .expect("tasks");
    drop(child);
    let scope = SessionReadScope::Descendant {
        root_session_id: SessionId("history".into()),
        ancestry: vec![SessionReadAncestor {
            subagent_id: rw_types::SubagentId("agent".into()),
            session_id: SessionId("child".into()),
            source_sequence: SequenceId(2),
        }],
    };
    (root, engine, scope)
}

#[tokio::test]
async fn historical_child_reads_require_effective_source_ancestry_and_reject_rewind_removal() {
    let (root, engine, scope) = descendant_fixture();
    let command = |scope| ClientCommand::GetTodos {
        meta: meta("child-tasks"),
        session_id: SessionId("child".into()),
        scope,
    };
    assert!(
        engine
            .query(command(SessionReadScope::Session {}))
            .await
            .is_err()
    );
    let mut wrong = scope.clone();
    if let SessionReadScope::Descendant { ancestry, .. } = &mut wrong {
        ancestry[0].source_sequence = SequenceId(1);
    }
    assert!(engine.query(command(wrong)).await.is_err());
    let result = engine
        .query(command(scope.clone()))
        .await
        .expect("authorized tasks");
    assert!(
        matches!(result.events(), [EngineEvent::TodosRead { session_id, result: rw_types::todo::TodoReadResult::Ready { todos }, .. }]
        if session_id.0 == "child" && todos.through == Some(SequenceId(1)))
    );
    let mut page_command = read("child-page");
    if let ClientCommand::ReadTranscript {
        session_id,
        scope: target_scope,
        ..
    } = &mut page_command
    {
        *session_id = SessionId("child".into());
        *target_scope = scope.clone();
    }
    let result = engine.query(page_command).await.expect("child page");
    let [
        EngineEvent::TranscriptPageReady {
            result: TranscriptReadResult::Ready { page },
            ..
        },
    ] = result.events()
    else {
        panic!("page")
    };
    let source = match &page.items[0].content {
        rw_types::transcript::TranscriptContent::Conversation { blocks, .. } => match &blocks[0] {
            rw_types::transcript::TranscriptConversationBlock::Text { body } => body.source.clone(),
            _ => panic!("text"),
        },
        _ => panic!("conversation"),
    };
    let result = engine
        .query(ClientCommand::ReadTranscriptContent {
            meta: meta("child-body"),
            session_id: SessionId("child".into()),
            scope: scope.clone(),
            read: rw_types::transcript::TranscriptContentRead {
                view: page.view.clone(),
                source,
                offset: 0,
                max_bytes: 4096,
            },
        })
        .await
        .expect("source");
    assert!(
        matches!(result.events(), [EngineEvent::TranscriptContentReady { page, .. }] if page.text.contains("child body"))
    );
    let mut parent = SessionEventLog::open(root.path(), "history").expect("rewind parent");
    parent
        .append(EngineEvent::ConversationRewound {
            meta: descendant_meta("history", 3),
            to_agent_turn: 0,
            operation_id: "rewind-child".into(),
            unrestorable_paths: vec![],
        })
        .expect("rewind");
    drop(parent);
    let mut removed = false;
    for _ in 0..4 {
        let error = engine
            .query(command(scope.clone()))
            .await
            .expect_err("removed association");
        if error.to_string().contains("association is unavailable") {
            removed = true;
            break;
        }
        assert!(error.to_string().contains("catching up"));
    }
    assert!(removed, "rewind must revoke the effective child source");
}

#[tokio::test]
async fn tail_reads_are_direct_and_reject_a_foreign_root() {
    use rw_types::transcript_tail::*;
    let (_root, engine) = fixture(1);
    let command = ClientCommand::ReadTranscriptTail {
        meta: meta("tail"),
        session_id: SessionId("history".into()),
        scope: SessionReadScope::Session {},
        read: TranscriptTailRead {
            expected: None,
            part: TranscriptTailPart::Text {},
            max_items: 1,
            max_bytes: u32::try_from(TRANSCRIPT_TAIL_MIN_PAGE_BYTES).expect("byte limit"),
        },
    };
    let reply = engine
        .dispatch(ClientId("bound".into()), command.clone())
        .await
        .expect("reply");
    let decoded: CommandReply = serde_json::from_slice(&reply.bytes).expect("typed reply");
    assert!(
        matches!(decoded, CommandReply::Read { events, outcome: CommandOutcome::Accepted {} } if matches!(&events[..], [EngineEvent::TranscriptTailReady { session_id, result: TranscriptTailResult::Ready { page }, .. }] if session_id.0 == "history" && page.view.through == Some(SequenceId(0))))
    );
    drop(reply);
    let ClientCommand::ReadTranscriptTail { meta, read, .. } = command else {
        panic!("tail");
    };
    assert!(
        engine
            .query(ClientCommand::ReadTranscriptTail {
                meta,
                session_id: SessionId("foreign".into()),
                scope: SessionReadScope::Session {},
                read
            })
            .await
            .is_err()
    );
}
