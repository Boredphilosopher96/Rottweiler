#![allow(clippy::expect_used)]

use super::*;
use crate::journal_reads::JournalRegistration;
use rw_store::session::SessionEventLog;
use rw_types::{
    Block, EngineEvent, EventMeta, PROTOCOL_VERSION, Role, Turn, TurnMeta,
    transcript::{
        TranscriptAnchor, TranscriptGeneration, TranscriptInvalidation, TranscriptItemId,
        TranscriptOrdinal, TranscriptPage, TranscriptPosition,
    },
};

struct Fixture {
    _root: tempfile::TempDir,
    journal: SessionEventLog,
    registration: JournalRegistration,
    service: Arc<TranscriptReader>,
}
impl Fixture {
    fn new(count: u64, text: &str) -> Self {
        let root = tempfile::tempdir().expect("root");
        let journals = JournalReads::new(root.path()).expect("read owner");
        let mut journal = SessionEventLog::open(root.path(), "semantic").expect("journal");
        journal
            .append_batch(
                (0..count).map(|sequence| EngineEvent::ConversationTurnCommitted {
                    meta: meta(sequence),
                    agent_turn: sequence,
                    turn: Turn {
                        role: Role::User,
                        blocks: vec![Block::Text {
                            text: format!("{sequence}:{text}"),
                        }],
                        meta: TurnMeta::default(),
                    },
                }),
            )
            .expect("events");
        let registration = journals
            .register("semantic", journal.read_view())
            .expect("registration");
        Self {
            _root: root,
            journal,
            registration,
            service: TranscriptReader::new(journals),
        }
    }
    fn read(
        &self,
        position: TranscriptPosition,
        known: Option<rw_types::transcript::TranscriptView>,
        bytes: u32,
    ) -> TranscriptReadResult {
        self.service
            .read(
                &SessionId("semantic".into()),
                &TranscriptRead {
                    known_view: known,
                    position,
                    max_items: 10,
                    max_bytes: bytes,
                },
            )
            .expect("page request")
    }
}
fn meta(sequence: u64) -> EventMeta {
    EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("semantic".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-04T00:00:00Z".into(),
        caused_by: None,
    }
}
fn ready(result: TranscriptReadResult) -> TranscriptPage {
    let TranscriptReadResult::Ready { page } = result else {
        panic!("expected complete page")
    };
    page
}

#[tokio::test]
async fn cancelled_waiters_cannot_release_running_worker_admission() {
    let fixture = Fixture::new(1, "body");
    let service = fixture.service;
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let mut waiters = Vec::new();
    for _ in 0..MAX_OPEN_PROJECTORS {
        let service = Arc::clone(&service);
        let gate = Arc::clone(&gate);
        let started = started.clone();
        waiters.push(tokio::spawn(async move {
            service
                .blocking(move |_| {
                    started.send(()).expect("start signal");
                    let (lock, changed) = &*gate;
                    let mut released = lock.lock().expect("gate");
                    while !*released {
                        released = changed.wait(released).expect("gate wait");
                    }
                    Ok(())
                })
                .await
        }));
    }
    for _ in 0..MAX_OPEN_PROJECTORS {
        starts.recv().await.expect("worker started");
    }
    for waiter in waiters {
        waiter.abort();
        assert!(waiter.await.expect_err("cancelled waiter").is_cancelled());
    }
    let exhausted: Result<(), HostError> =
        service.blocking(|_| panic!("unadmitted backend ran")).await;
    let (lock, changed) = &*gate;
    *lock.lock().expect("release gate") = true;
    changed.notify_all();
    assert!(matches!(exhausted, Err(HostError::Query(_))));
    let settled = Arc::clone(&service.workers)
        .acquire_many_owned(u32::try_from(MAX_OPEN_PROJECTORS).expect("worker count"))
        .await
        .expect("all workers settled");
    drop(settled);
    service
        .blocking(|_| Ok(()))
        .await
        .expect("admission restored");
}

#[test]
fn catch_up_is_bounded_and_first_middle_latest_are_indexed_current_views() {
    let fixture = Fixture::new(300, "body");
    assert!(matches!(
        fixture.read(TranscriptPosition::Latest, None, 64 * 1024),
        TranscriptReadResult::CatchingUp {
            through: Some(SequenceId(255)),
            target: Some(SequenceId(299))
        }
    ));
    let latest = ready(fixture.read(TranscriptPosition::Latest, None, 64 * 1024));
    assert_eq!(latest.first_ordinal, TranscriptOrdinal(290));
    assert_eq!(
        latest.items.last().expect("tail").id,
        TranscriptItemId(SequenceId(299))
    );
    let middle = ready(fixture.read(
        TranscriptPosition::AtOrdinal {
            ordinal: TranscriptOrdinal(150),
            generation: latest.view.generation,
        },
        Some(latest.view.clone()),
        64 * 1024,
    ));
    assert_eq!(middle.items[0].ordinal, TranscriptOrdinal(150));
    assert_eq!(middle.invalidation, TranscriptInvalidation::None);
    assert_eq!(middle.view, latest.view);
    let first = ready(fixture.read(TranscriptPosition::First, Some(latest.view), 64 * 1024));
    assert_eq!(first.items[0].id, TranscriptItemId(SequenceId(0)));
}

#[tokio::test]
async fn offline_reader_uses_the_same_projection_without_a_session_actor() {
    let Fixture {
        _root: root,
        journal,
        registration,
        service,
    } = Fixture::new(300, "offline");
    drop(registration);
    drop(journal);
    drop(service);
    let reader = TranscriptReader::open(root.path()).expect("offline reader");
    let session = SessionId("semantic".into());
    let bootstrap = reader
        .bootstrap(session.clone())
        .await
        .expect("bounded header");
    assert!(bootstrap.created.is_none());
    assert_eq!(bootstrap.through_sequence, Some(SequenceId(299)));
    let request = TranscriptRead {
        known_view: None,
        position: TranscriptPosition::Latest,
        max_items: 8,
        max_bytes: 64 * 1024,
    };
    assert!(matches!(
        reader
            .page(session.clone(), request.clone())
            .await
            .expect("catchup"),
        TranscriptReadResult::CatchingUp { .. }
    ));
    let page = ready(reader.page(session, request).await.expect("page"));
    assert_eq!(page.first_ordinal, TranscriptOrdinal(292));
    assert_eq!(
        page.items.last().expect("last").id,
        TranscriptItemId(SequenceId(299))
    );
}

#[test]
fn byte_limited_latest_keeps_the_last_item_and_before_excludes_its_anchor() {
    let fixture = Fixture::new(12, &"quoted \\\" λ".repeat(200));
    let latest = ready(fixture.read(TranscriptPosition::Latest, None, 8192));
    assert!(latest.items.len() < 10);
    assert_eq!(
        latest.items.last().expect("last").id,
        TranscriptItemId(SequenceId(11))
    );
    assert!(serde_json::to_vec(&latest).expect("wire page").len() <= 8192);
    let before = ready(fixture.read(
        TranscriptPosition::Before {
            item: TranscriptItemId(SequenceId(1)),
        },
        Some(latest.view),
        8192,
    ));
    assert_eq!(before.items.len(), 1);
    assert_eq!(before.items[0].id, TranscriptItemId(SequenceId(0)));
}

#[test]
fn rewind_changes_ordering_and_recovers_a_removed_stable_anchor() {
    let mut fixture = Fixture::new(10, "body");
    let old = ready(fixture.read(TranscriptPosition::Latest, None, 64 * 1024));
    fixture
        .journal
        .append(&EngineEvent::ConversationRewound {
            meta: meta(10),
            to_agent_turn: 3,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        })
        .expect("rewind");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    let result = fixture.read(
        TranscriptPosition::AtOrdinal {
            ordinal: TranscriptOrdinal(8),
            generation: old.view.generation,
        },
        Some(old.view.clone()),
        64 * 1024,
    );
    assert!(
        matches!(result, TranscriptReadResult::OrderingChanged { view } if view.generation == TranscriptGeneration(1))
    );
    let page = ready(fixture.read(
        TranscriptPosition::Around {
            item: TranscriptItemId(SequenceId(8)),
        },
        Some(old.view),
        64 * 1024,
    ));
    assert!(matches!(
        page.anchor,
        TranscriptAnchor::Replaced {
            requested: TranscriptItemId(SequenceId(8)),
            replacement: Some(TranscriptItemId(SequenceId(3)))
        }
    ));
    assert_eq!(page.invalidation, TranscriptInvalidation::All);
    assert_eq!(page.total_items, TranscriptOrdinal(4));
}

#[test]
fn projector_admission_never_evicts_an_active_operation() {
    let fixture = Fixture::new(0, "");
    let held = (0..MAX_OPEN_PROJECTORS)
        .map(|index| {
            fixture
                .service
                .projector(&SessionId(format!("held-{index}")))
                .expect("owner")
        })
        .collect::<Vec<_>>();
    assert!(
        fixture
            .service
            .projector(&SessionId("extra".into()))
            .is_err()
    );
    drop(held);
    assert!(
        fixture
            .service
            .projector(&SessionId("extra".into()))
            .is_ok()
    );
    assert_eq!(
        fixture.service.projectors.lock().expect("cache").len(),
        MAX_OPEN_PROJECTORS
    );
}

#[test]
fn late_tool_final_invalidates_its_stable_item_without_changing_order() {
    let mut fixture = Fixture::new(1, "tool request");
    fixture
        .journal
        .append(&EngineEvent::ToolCallStarted {
            meta: meta(1),
            turn_id: rw_types::TurnId("1".into()),
            tool_call_id: rw_types::ToolCallId("provider-id".into()),
            invocation_id: rw_types::ToolInvocationId("host-invocation".into()),
            name: "read_file".into(),
            args: serde_json::json!({"path":"file.rs"}),
            call_index: 0,
        })
        .expect("start");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    let before = ready(fixture.read(TranscriptPosition::Latest, None, 64 * 1024));
    fixture
        .journal
        .append(&EngineEvent::ToolCallFinished {
            meta: meta(2),
            turn_id: rw_types::TurnId("1".into()),
            tool_call_id: rw_types::ToolCallId("provider-id".into()),
            invocation_id: rw_types::ToolInvocationId("host-invocation".into()),
            output: rw_types::ToolOutput::Text {
                text: "finished content".into(),
            },
            is_error: false,
            call_index: 0,
        })
        .expect("finish");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    let after = ready(fixture.read(
        TranscriptPosition::Latest,
        Some(before.view.clone()),
        64 * 1024,
    ));
    assert_eq!(after.view.generation, before.view.generation);
    assert_eq!(after.items.len(), before.items.len());
    assert_eq!(after.items[1].id, before.items[1].id);
    assert_eq!(after.items[1].revision, SequenceId(2));
    assert_eq!(
        after.invalidation,
        TranscriptInvalidation::Items {
            items: vec![TranscriptItemId(SequenceId(1))]
        }
    );
}

#[test]
fn malformed_limits_are_rejected_before_projector_or_journal_admission() {
    let fixture = Fixture::new(0, "");
    assert!(
        fixture
            .service
            .read(
                &SessionId("semantic".into()),
                &TranscriptRead {
                    known_view: None,
                    position: TranscriptPosition::Latest,
                    max_items: u32::MAX,
                    max_bytes: 4096,
                }
            )
            .is_err()
    );
    assert!(fixture.service.projectors.lock().expect("cache").is_empty());
}

#[test]
fn incompatible_derived_version_rebuilds_without_changing_canonical_history() {
    let fixture = Fixture::new(3, "canonical");
    let view = fixture.journal.read_view();
    let prefix = view.prefix_identity();
    drop(
        rw_store::session::transcript_index::TranscriptIndex::open(&view, 999)
            .expect("old derived version"),
    );
    let page = ready(fixture.read(TranscriptPosition::Latest, None, 64 * 1024));
    assert_eq!(page.items.len(), 3);
    assert_eq!(
        page.view.projection_version,
        rw_types::transcript::TRANSCRIPT_PROJECTION_VERSION
    );
    assert_eq!(fixture.journal.read_view().prefix_identity(), prefix);
}

#[test]
fn complete_content_chunks_reuse_one_canonical_document_and_validate_view_boundaries() {
    let text = "λ🐕\"\n".repeat(1200);
    let fixture = Fixture::new(1, &text);
    let page = ready(fixture.read(TranscriptPosition::Latest, None, 64 * 1024));
    let request = rw_types::transcript::TranscriptContentRead {
        view: page.view,
        source: rw_types::transcript::TranscriptContentSource {
            sequence: SequenceId(0),
            selector: rw_types::transcript::TranscriptContentSelector::ConversationBlock {
                index: 0,
            },
        },
        offset: 0,
        max_bytes: 101,
    };
    let mut complete = String::new();
    let mut next = request.clone();
    loop {
        let chunk = fixture
            .service
            .read_content(&SessionId("semantic".into()), &next)
            .expect("chunk");
        assert!(chunk.text.len() <= 101);
        complete.push_str(&chunk.text);
        let Some(offset) = chunk.next_offset else {
            break;
        };
        next.offset = offset;
    }
    assert_eq!(complete, format!("0:{text}"));
    assert_eq!(
        fixture
            .service
            .documents
            .lock()
            .expect("documents")
            .build_count(),
        1
    );
    let mut bad = request.clone();
    bad.view.session_id = SessionId("foreign".into());
    assert!(
        fixture
            .service
            .read_content(&SessionId("semantic".into()), &bad)
            .is_err()
    );
    bad = request.clone();
    bad.offset = 3; // Inside the first lambda after the two-byte prefix.
    assert!(
        fixture
            .service
            .read_content(&SessionId("semantic".into()), &bad)
            .is_err()
    );
    bad = request;
    bad.source.sequence = SequenceId(1);
    assert!(
        fixture
            .service
            .read_content(&SessionId("semantic".into()), &bad)
            .is_err()
    );
}
