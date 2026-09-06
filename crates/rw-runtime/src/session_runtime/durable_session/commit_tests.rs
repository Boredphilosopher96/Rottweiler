#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{DurableEventSink, JournalService};
use rw_core::{SESSION_EVENT_VERSION, SessionEventSink, commit_session_events};
use rw_store::session::{SessionEventLog, SessionEventPageLimits};
use rw_types::{ClientId, EngineEvent, EventMeta, SequenceId, SessionId};
use std::{sync::Arc, time::Duration};

fn event(sequence: u64) -> EngineEvent {
    EngineEvent::SessionCreated {
        meta: EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: SessionId("ordered".to_owned()),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-09-05T00:00:00Z".to_owned(),
            caused_by: None,
        },
        driver_client_id: ClientId("driver".to_owned()),
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_native_commit_keeps_order_and_published_reads_do_not_wait_for_writer_io() {
    let root = tempfile::tempdir().expect("root");
    let service = JournalService::new(root.path()).expect("service");
    let sink = DurableEventSink::new(
        SessionEventLog::open(root.path(), "ordered").expect("log"),
        root.path().to_owned(),
        "ordered".to_owned(),
        Arc::clone(&service),
    )
    .expect("sink");
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let writer = {
        let log = Arc::clone(&sink.log);
        std::thread::spawn(move || {
            let _guard = log.lock().expect("writer lock");
            locked.send(()).expect("locked");
            wait.recv_timeout(Duration::from_secs(5))
                .expect("release writer");
        })
    };
    ready.await.expect("writer locked");
    let caller = tokio::spawn(commit_session_events(Arc::clone(&sink), vec![event(0)]));
    tokio::time::timeout(Duration::from_secs(2), async {
        while service.commits.pending_jobs() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("commit accepted");
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
    let published = tokio::spawn({
        let sink = Arc::clone(&sink);
        async move { sink.last_sequence().await }
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), published)
            .await
            .expect("published read independent of writer IO")
            .expect("reader task")
            .expect("tail"),
        None
    );
    let next = tokio::spawn(commit_session_events(Arc::clone(&sink), vec![event(1)]));
    tokio::task::yield_now().await;
    assert!(!next.is_finished());
    release.send(()).expect("release");
    writer.join().expect("writer owner");
    tokio::time::timeout(Duration::from_secs(3), next)
        .await
        .expect("next commit")
        .expect("task")
        .expect("durable receipt");
    sink.settle_effects().await.expect("session settled");
    let page = sink
        .capture_read_view()
        .expect("view")
        .read_page(None, rw_core::SessionReplayLimits::default())
        .await
        .expect("page");
    assert_eq!(page, vec![event(0), event(1)]);
    assert_eq!(
        service
            .capture("ordered")
            .expect("published")
            .view
            .page::<EngineEvent>(None, SessionEventPageLimits::default())
            .expect("source")
            .events
            .len(),
        2
    );
    service.commits.shutdown().await.expect("service settled");
}
