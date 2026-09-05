use super::fixtures::models::ScriptedModel;
use super::fixtures::sinks::BlockingBatchSink;
use super::fixtures::support::{FixedClock, collect_turn, config, protocol_meta, stop_script};
use crate::engine::AgentTurnStatus;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::SessionControl;
use rw_tools::{CancellationToken, ToolRegistry};
use rw_types::config::PermissionDecision;
use rw_types::{ClientCommand, ClientId, CommandOutcome, EngineEvent, SessionId};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn interrupt_acknowledges_while_opening_journal_commit_is_blocked() {
    let root = tempfile::TempDir::new().expect("workspace");
    let sink = Arc::new(BlockingBatchSink {
        persisted: std::sync::Mutex::default(),
        blocked_once: std::sync::atomic::AtomicBool::default(),
        entered: tokio::sync::Notify::default(),
        release: tokio::sync::Notify::default(),
    });
    let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
    let mut cfg = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        rw_ext::HookDispatcher::new(),
    );
    cfg.event_sink = sink.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(cfg)
        .await
        .expect("actor");
    handle.ensure_local_driver().await.expect("driver");
    let mut events = handle.subscribe().expect("events");
    let sender = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.send_message("first").await })
    };
    tokio::time::timeout(Duration::from_secs(3), sink.entered.notified())
        .await
        .expect("commit blocked");
    let mut invalid = protocol_meta("local", "invalid-protocol");
    invalid.protocol_version += 1;
    for (meta, session_id) in [
        (
            protocol_meta("observer", "observer"),
            handle.session_id().clone(),
        ),
        (invalid, handle.session_id().clone()),
        (
            protocol_meta("local", "wrong-session"),
            SessionId("other".to_owned()),
        ),
    ] {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            handle.dispatch(ClientCommand::Interrupt { meta, session_id }),
        )
        .await
        .expect("bounded rejection")
        .expect("dispatch");
        assert!(matches!(result, CommandOutcome::Rejected { .. }));
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(1), handle.interrupt())
            .await
            .expect("interrupt does not wait for storage")
            .expect("interrupt")
    );
    assert!(model.requests.lock().expect("requests").is_empty());
    sink.release.notify_one();
    sender.await.expect("sender").expect("message");
    let first = collect_turn(&mut events).await;
    assert!(first.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    )));
    assert!(model.requests.lock().expect("requests").is_empty());
    handle.send_message("second").await.expect("second turn");
    let second = collect_turn(&mut events).await;
    assert!(second.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        }
    )));
    handle.close().await.expect("close");
}

#[test]
fn control_uses_committed_lease_and_never_retargets_an_admitted_interrupt() {
    let session = SessionId("session".to_owned());
    let control = SessionControl::new(
        session.clone(),
        Some(ClientId("driver".to_owned())),
        Arc::new(FixedClock),
    );
    let (events, mut received) = tokio::sync::broadcast::channel(16);
    let first = CancellationToken::default();
    control.start(1, first.clone());
    assert!(matches!(
        control.interrupt(&protocol_meta("next", "not-committed"), &session, &events),
        CommandOutcome::Rejected { .. }
    ));
    assert!(!first.is_cancelled());
    assert_eq!(
        control.interrupt(&protocol_meta("driver", "cancel-first"), &session, &events),
        CommandOutcome::Accepted {}
    );
    assert!(first.is_cancelled());
    control.commit_driver(Some(ClientId("next".to_owned())));
    let second = CancellationToken::default();
    control.start(2, second.clone());
    control.finish(1);
    // Reading the acknowledgement has no cancellation work attached to it.
    assert!(matches!(
        received.try_recv().expect("rejected ack").event,
        EngineEvent::CommandAcknowledged {
            outcome: CommandOutcome::Rejected { .. },
            ..
        }
    ));
    assert!(matches!(
        received.try_recv().expect("accepted ack").event,
        EngineEvent::CommandAcknowledged {
            outcome: CommandOutcome::Accepted {},
            ..
        }
    ));
    assert!(!second.is_cancelled());
    assert!(matches!(
        control.interrupt(&protocol_meta("driver", "stale-driver"), &session, &events),
        CommandOutcome::Rejected { .. }
    ));
    assert!(!second.is_cancelled());
    control.close();
    assert!(second.is_cancelled());
    assert!(matches!(
        control.interrupt(&protocol_meta("next", "closed"), &session, &events),
        CommandOutcome::Rejected { .. }
    ));
    let after_close = CancellationToken::default();
    control.start(3, after_close.clone());
    assert!(after_close.is_cancelled());
}
