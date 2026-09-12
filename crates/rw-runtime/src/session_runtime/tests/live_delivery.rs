//! The actual writer's maximum JSONL record must fit the subscription read policy.
use super::{DurableEventSink, JournalService, SessionEventLog, SessionEventSink};
use rw_core::SessionReplayLimits;
use rw_store::session::journal::{JournalAppendPlan, MAX_JOURNAL_APPEND_BYTES};
use rw_types::{EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, TurnId};

#[tokio::test]
async fn maximum_legal_jsonl_record_replays_with_delivery_page_limits() {
    let storage = tempfile::tempdir().expect("storage");
    let session = SessionId("maximum-delivery".into());
    let event = |text| EngineEvent::TextDelta {
        meta: EventMeta {
            protocol_version: PROTOCOL_VERSION,
            session_id: session.clone(),
            sequence_id: SequenceId(0),
            emitted_at: "2026-09-06T00:00:00.000Z".into(),
            caused_by: None,
        },
        turn_id: TurnId("1".into()),
        text,
    };
    let empty = event(String::new());
    let overhead = JournalAppendPlan::measure(SequenceId(0), &[empty])
        .expect("envelope size")
        .encoded_bytes();
    let encoded_text = MAX_JOURNAL_APPEND_BYTES - overhead;
    // Escaped bytes reach the actual wire boundary without exceeding the
    // independent decoded-object admission. An ASCII 16 MiB string is not that oracle.
    let text = "\0".repeat(encoded_text / 6) + &"x".repeat(encoded_text % 6);
    let expected = event(text);
    assert_eq!(
        JournalAppendPlan::measure(SequenceId(0), std::slice::from_ref(&expected))
            .expect("maximum line")
            .encoded_bytes(),
        MAX_JOURNAL_APPEND_BYTES
    );
    let mut log = SessionEventLog::open(storage.path(), &session.0).expect("journal");
    log.append(expected.clone())
        .expect("legal maximum record commits");
    let sink = DurableEventSink::new(
        log,
        storage.path().to_owned(),
        session.0.clone(),
        JournalService::new(storage.path()).expect("read owner"),
    )
    .expect("durable sink");
    let view = sink.capture_read_view().expect("captured source");
    let page = view
        .read_page(None, SessionReplayLimits::live_delivery())
        .await
        .expect("maximum source replays");
    assert_eq!(page, vec![expected]);
    assert_eq!(view.last_sequence(), Some(SequenceId(0)));
    sink.settle_effects().await.expect("settled reads");
}
