#![allow(clippy::expect_used)]
use super::*;
use crate::server::ServerEngine as _;
use rw_store::session::SessionEventLog;
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
        meta: meta(id),
        session_id: SessionId("history".into()),
        read: TranscriptRead {
            known_view: None,
            position: TranscriptPosition::Latest,
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
    assert!(matches!(
        events.recv().await,
        Some(Ok(EngineEvent::SessionHistoryReady {
            through_sequence: Some(SequenceId(299)),
            ..
        }))
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
        outcome: CommandOutcome::Accepted,
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
        assert_eq!(reply.outcome == CommandOutcome::Accepted, accepted);
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
