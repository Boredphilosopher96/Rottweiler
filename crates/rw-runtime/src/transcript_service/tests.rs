#![allow(clippy::expect_used)]

use super::*;
use crate::journal_service::JournalRegistration;
use rw_store::session::SessionEventLog;
use rw_types::{
    EngineEvent, EventMeta, PROTOCOL_VERSION,
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
        let journals = JournalService::new(root.path()).expect("read owner");
        let mut journal = SessionEventLog::open(root.path(), "semantic").expect("journal");
        journal
            .append_batch((0..count).flat_map(|ordinal| {
                crate::session_runtime::test_history::input_events(
                    meta(2 * ordinal),
                    ordinal,
                    format!("{ordinal}:{text}"),
                )
            }))
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
                &rw_types::session_read::SessionReadScope::Session {},
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
        fixture.read(TranscriptPosition::Latest {}, None, 64 * 1024),
        TranscriptReadResult::CatchingUp {
            through: Some(SequenceId(255)),
            target: Some(SequenceId(599))
        }
    ));
    assert!(matches!(
        fixture.read(TranscriptPosition::Latest {}, None, 64 * 1024),
        TranscriptReadResult::CatchingUp {
            through: Some(SequenceId(511)),
            target: Some(SequenceId(599))
        }
    ));
    let latest = ready(fixture.read(TranscriptPosition::Latest {}, None, 64 * 1024));
    assert_eq!(latest.first_ordinal, TranscriptOrdinal(290));
    assert_eq!(
        latest.items.last().expect("tail").id,
        TranscriptItemId(SequenceId(599))
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
    assert_eq!(middle.invalidation, TranscriptInvalidation::None {});
    assert_eq!(middle.view, latest.view);
    let first = ready(fixture.read(TranscriptPosition::First {}, Some(latest.view), 64 * 1024));
    assert_eq!(first.items[0].id, TranscriptItemId(SequenceId(1)));
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
    assert_eq!(bootstrap.through_sequence, Some(SequenceId(599)));
    let request = TranscriptRead {
        known_view: None,
        position: TranscriptPosition::Latest {},
        max_items: 8,
        max_bytes: 64 * 1024,
    };
    assert!(matches!(
        reader
            .page(
                session.clone(),
                rw_types::session_read::SessionReadScope::Session {},
                request.clone()
            )
            .await
            .expect("catchup"),
        TranscriptReadResult::CatchingUp { .. }
    ));
    assert!(matches!(
        reader
            .page(
                session.clone(),
                rw_types::session_read::SessionReadScope::Session {},
                request.clone()
            )
            .await
            .expect("second bounded page"),
        TranscriptReadResult::CatchingUp {
            through: Some(SequenceId(511)),
            target: Some(SequenceId(599))
        }
    ));
    let page = ready(
        reader
            .page(
                session,
                rw_types::session_read::SessionReadScope::Session {},
                request,
            )
            .await
            .expect("page"),
    );
    assert_eq!(page.first_ordinal, TranscriptOrdinal(292));
    assert_eq!(
        page.items.last().expect("last").id,
        TranscriptItemId(SequenceId(599))
    );
}

#[test]
fn byte_limited_latest_keeps_the_last_item_and_before_excludes_its_anchor() {
    let fixture = Fixture::new(12, &"quoted \\\" λ".repeat(200));
    let latest = ready(fixture.read(TranscriptPosition::Latest {}, None, 8192));
    assert!(latest.items.len() < 10);
    assert_eq!(
        latest.items.last().expect("last").id,
        TranscriptItemId(SequenceId(23))
    );
    assert!(serde_json::to_vec(&latest).expect("wire page").len() <= 8192);
    let before = ready(fixture.read(
        TranscriptPosition::Before {
            item: TranscriptItemId(SequenceId(3)),
        },
        Some(latest.view),
        8192,
    ));
    assert_eq!(before.items.len(), 1);
    assert_eq!(before.items[0].id, TranscriptItemId(SequenceId(1)));
}

#[test]
fn rewind_changes_ordering_and_recovers_a_removed_stable_anchor() {
    let mut fixture = Fixture::new(10, "body");
    let old = ready(fixture.read(TranscriptPosition::Latest {}, None, 64 * 1024));
    fixture
        .journal
        .append(&EngineEvent::ConversationRewound {
            meta: meta(20),
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
            item: TranscriptItemId(SequenceId(17)),
        },
        Some(old.view),
        64 * 1024,
    ));
    assert!(matches!(
        page.anchor,
        TranscriptAnchor::Replaced {
            requested: TranscriptItemId(SequenceId(17)),
            replacement: Some(TranscriptItemId(SequenceId(7)))
        }
    ));
    assert_eq!(page.invalidation, TranscriptInvalidation::All {});
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
            meta: meta(2),
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
    let before = ready(fixture.read(TranscriptPosition::Latest {}, None, 64 * 1024));
    fixture
        .journal
        .append(&EngineEvent::ToolCallFinished {
            presentation: None,
            meta: meta(3),
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
        TranscriptPosition::Latest {},
        Some(before.view.clone()),
        64 * 1024,
    ));
    assert_eq!(after.view.generation, before.view.generation);
    assert_eq!(after.items.len(), before.items.len());
    assert_eq!(after.items[1].id, before.items[1].id);
    assert_eq!(after.items[1].revision, SequenceId(3));
    assert_eq!(
        after.invalidation,
        TranscriptInvalidation::Items {
            items: vec![TranscriptItemId(SequenceId(2))]
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
                &rw_types::session_read::SessionReadScope::Session {},
                &TranscriptRead {
                    known_view: None,
                    position: TranscriptPosition::Latest {},
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
    let page = ready(fixture.read(TranscriptPosition::Latest {}, None, 64 * 1024));
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
    let page = ready(fixture.read(TranscriptPosition::Latest {}, None, 64 * 1024));
    let request = rw_types::transcript::TranscriptContentRead {
        view: page.view,
        source: rw_types::transcript::TranscriptContentSource {
            sequence: SequenceId(1),
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
            .read_content(
                &SessionId("semantic".into()),
                &rw_types::session_read::SessionReadScope::Session {},
                &next,
            )
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
            .read_content(
                &SessionId("semantic".into()),
                &rw_types::session_read::SessionReadScope::Session {},
                &bad
            )
            .is_err()
    );
    bad = request.clone();
    bad.offset = 3; // Inside the first lambda after the two-byte prefix.
    assert!(
        fixture
            .service
            .read_content(
                &SessionId("semantic".into()),
                &rw_types::session_read::SessionReadScope::Session {},
                &bad
            )
            .is_err()
    );
    bad = request;
    bad.source.sequence = SequenceId(0);
    assert!(
        fixture
            .service
            .read_content(
                &SessionId("semantic".into()),
                &rw_types::session_read::SessionReadScope::Session {},
                &bad
            )
            .is_err()
    );
}

#[tokio::test]
async fn tool_action_presentation_requires_exact_prefix_and_effective_invocation() {
    use rw_types::{ToolCallId, ToolInvocationId, ToolOutput, TurnId};
    let mut fixture = Fixture::new(0, "");
    let invocation = ToolInvocationId("tool-instance".into());
    let presentation = tool_presentation_fixture();
    fixture
        .journal
        .append_batch([
            EngineEvent::ToolCallStarted {
                meta: meta(0),
                turn_id: TurnId("1".into()),
                tool_call_id: ToolCallId("provider-reused".into()),
                invocation_id: invocation.clone(),
                name: "read".into(),
                args: serde_json::json!({}),
                call_index: 0,
            },
            EngineEvent::ToolCallFinished {
                meta: meta(1),
                turn_id: TurnId("1".into()),
                tool_call_id: ToolCallId("provider-reused".into()),
                invocation_id: invocation.clone(),
                output: ToolOutput::Text {
                    text: "done".into(),
                },
                presentation: Some(presentation.clone()),
                is_error: false,
                call_index: 0,
            },
        ])
        .expect("tool lifecycle");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    let session = SessionId("semantic".into());
    assert!(
        fixture
            .service
            .tool_presentation(session.clone(), invocation.clone(), None)
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .service
            .tool_presentation(session.clone(), invocation.clone(), Some(SequenceId(1)))
            .await
            .expect("exact source"),
        Some(presentation)
    );
    assert_eq!(
        fixture
            .service
            .tool_presentation(
                session.clone(),
                ToolInvocationId("different".into()),
                Some(SequenceId(1))
            )
            .await
            .expect("missing invocation"),
        None
    );
    fixture
        .journal
        .append(&EngineEvent::ConversationRewound {
            meta: meta(2),
            to_agent_turn: 0,
            operation_id: "rewind-source".into(),
            unrestorable_paths: vec![],
        })
        .expect("rewind");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    assert!(
        fixture
            .service
            .tool_presentation(session.clone(), invocation.clone(), Some(SequenceId(1)))
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .service
            .tool_presentation(session, invocation, Some(SequenceId(2)))
            .await
            .expect("removed effective source"),
        None
    );
}

fn tool_presentation_fixture() -> rw_types::extension_ui::UiPresentation {
    use rw_types::extension_ui::{
        UiContribution, UiContributionOwner, UiGenerationId, UiPresentation,
    };
    UiPresentation::project(
        UiContributionOwner {
            extension: "example".into(),
            generation: UiGenerationId::from_bytes([1; 16]),
        },
        &UiContribution::Tool {
            id: "result".into(),
            tool_name: "read".into(),
            title: "Read result".into(),
            fields: vec![],
            actions: vec![],
        },
        &serde_json::json!({}),
    )
    .expect("presentation")
}

#[tokio::test]
async fn retained_tail_results_keep_admission_until_query_consumption() {
    use rw_types::transcript_tail::{
        TRANSCRIPT_TAIL_MIN_PAGE_BYTES, TranscriptTailPart, TranscriptTailRead,
    };
    let fixture = Fixture::new(1, "bounded");
    let request = || TranscriptTailRead {
        expected: None,
        part: TranscriptTailPart::Text {},
        max_items: 1,
        max_bytes: u32::try_from(TRANSCRIPT_TAIL_MIN_PAGE_BYTES).expect("wire budget"),
    };
    let mut retained = Vec::new();
    for _ in 0..MAX_OPEN_PROJECTORS {
        let result = fixture
            .service
            .tail(
                SessionId("semantic".into()),
                rw_types::session_read::SessionReadScope::Session {},
                request(),
            )
            .await
            .expect("admitted tail");
        assert!(matches!(
            result.value(),
            rw_types::transcript_tail::TranscriptTailResult::Ready { .. }
        ));
        retained.push(result);
    }
    assert_eq!(fixture.service.workers.available_permits(), 0);
    assert!(
        fixture
            .service
            .tail(
                SessionId("semantic".into()),
                rw_types::session_read::SessionReadScope::Session {},
                request()
            )
            .await
            .is_err()
    );
    let result = retained.pop().expect("one retained");
    let query = result.into_query(|result| EngineEvent::TranscriptTailReady {
        meta: rw_types::CommandAckMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: rw_types::ClientId("client".into()),
            request_id: rw_types::RequestId("tail".into()),
            emitted_at: "2026-01-01T00:00:00Z".into(),
        },
        session_id: SessionId("semantic".into()),
        result,
    });
    assert_eq!(fixture.service.workers.available_permits(), 0);
    drop(query);
    assert_eq!(fixture.service.workers.available_permits(), 1);
    drop(retained);
    assert_eq!(
        fixture.service.workers.available_permits(),
        MAX_OPEN_PROJECTORS
    );
}

#[tokio::test]
async fn active_children_query_is_mode_free_bounded_and_source_qualified() {
    let mut fixture = Fixture::new(80, "prior body");
    fixture
        .journal
        .append(EngineEvent::TurnStarted {
            meta: meta(160),
            turn_id: rw_types::TurnId("80".into()),
        })
        .expect("active parent turn");
    fixture
        .journal
        .append(EngineEvent::SubagentSpawned {
            meta: meta(161),
            subagent_id: rw_types::SubagentId("agent".into()),
            child_session_id: SessionId("child".into()),
            task: "€".repeat(1024),
        })
        .expect("spawn");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    let scope = rw_types::session_read::SessionReadScope::Session {};
    let first = fixture
        .service
        .children(SessionId("semantic".into()), scope.clone())
        .await
        .expect("bounded batch");
    assert!(matches!(
        first.value(),
        rw_types::session_children::SessionChildrenResult::CatchingUp {
            through: Some(SequenceId(63)),
            target: Some(SequenceId(161))
        }
    ));
    drop(first);
    let second = fixture
        .service
        .children(SessionId("semantic".into()), scope.clone())
        .await
        .expect("second bounded batch");
    assert!(matches!(
        second.value(),
        rw_types::session_children::SessionChildrenResult::CatchingUp {
            through: Some(SequenceId(127)),
            target: Some(SequenceId(161))
        }
    ));
    drop(second);
    let ready = fixture
        .service
        .children(SessionId("semantic".into()), scope.clone())
        .await
        .expect("ready");
    let rw_types::session_children::SessionChildrenResult::Ready { snapshot } = ready.value()
    else {
        panic!("snapshot")
    };
    assert_eq!(snapshot.through, Some(SequenceId(161)));
    assert_eq!(snapshot.children.len(), 1);
    assert_eq!(snapshot.children[0].spawned, SequenceId(161));
    assert_eq!(snapshot.children[0].child_session_id.0, "child");
    assert!(snapshot.children[0].task_truncated);
    assert!(
        snapshot.children[0].task_preview.len()
            <= rw_types::session_children::MAX_CHILD_TASK_PREVIEW_BYTES
    );
    let wrong = rw_types::session_read::SessionReadScope::Descendant {
        root_session_id: SessionId("semantic".into()),
        ancestry: vec![rw_types::session_read::SessionReadAncestor {
            session_id: SessionId("child".into()),
            subagent_id: rw_types::SubagentId("agent".into()),
            source_sequence: SequenceId(159),
        }],
    };
    assert!(
        fixture
            .service
            .children(SessionId("child".into()), wrong)
            .await
            .is_err()
    );
}
