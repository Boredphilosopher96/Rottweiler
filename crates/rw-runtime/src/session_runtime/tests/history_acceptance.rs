//! Explicit production-path history scaling; the executable is prebuilt, never self-compiles.
use super::{child_plugin_sessions::controller, dormant_controls};
use rw_core::{SubagentRecoveryPolicy, SubagentSessionFactory};
use rw_types::{
    EngineEvent, SequenceId, SessionId, SessionMode,
    session_read::SessionReadScope,
    transcript::{
        TranscriptContent, TranscriptContentRead, TranscriptContentSelector,
        TranscriptContentSource, TranscriptConversationBlock, TranscriptItemId, TranscriptPosition,
        TranscriptRead, TranscriptReadResult,
    },
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
mod source;
use source::{Expected, SESSION};
const SIZES: [u64; 3] = [100, 1_000, 10_000];
const REOPEN_SAMPLES: usize = 5;
const WARM_SAMPLES: usize = 5;
const FRESH_SAMPLES: usize = 3;
const SUBPROCESS_ROOT: &str = "RW_HISTORY_ACCEPTANCE_ROOT";
const TEST: &str = "session_runtime::tests::history_acceptance::qualify_cold_actor_source_history";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "release acceptance: 100/1k/10k mixed histories, explicit native helper, fresh subprocesses"]
async fn qualify_cold_actor_source_history() {
    assert!(
        !std::hint::black_box(cfg!(debug_assertions)),
        "prebuild this acceptance with --release"
    );
    if let Some(root) = std::env::var_os(SUBPROCESS_ROOT) {
        let root = std::path::PathBuf::from(root);
        let expected = expected(&root);
        sample(&root, &expected, "fresh_process_index_reopen", 0, false).await;
        return;
    }
    let mut fixtures = Vec::with_capacity(SIZES.len());
    for count in SIZES {
        let root = tempfile::tempdir().expect("fixture");
        std::fs::create_dir(root.path().join("workspace")).expect("workspace");
        let storage = root.path().join("state");
        crate::storage_root::initialize_private_storage_root(&storage).expect("private storage");
        let info = source::seed(&storage, count);
        let encoded = serde_json::to_vec(&info).expect("expected sources");
        assert!(encoded.len() < 16 * 1024);
        std::fs::write(root.path().join("expected.json"), encoded).expect("oracle");
        println!(
            "history_acceptance {}",
            serde_json::json!({"phase":"seed_complete","conversations":count,"source_events":info.next_sequence,"source_bytes":info.journal_bytes,"accepted_input_body_bytes":info.input_body_bytes,"seed_batch_max_events":16,"seed_is_timed":false})
        );
        fixtures.push((root, info));
    }
    for (root, info) in &fixtures {
        sample(
            root.path(),
            info,
            "first_index_build_and_actor_activation",
            0,
            true,
        )
        .await;
    }
    // Interleave sizes so the large source is paired with the small source at each sample.
    for ordinal in 0..REOPEN_SAMPLES {
        for (root, info) in &fixtures {
            sample(
                root.path(),
                info,
                "same_process_index_reopen",
                ordinal,
                false,
            )
            .await;
        }
    }
    for ordinal in 0..FRESH_SAMPLES {
        for (root, info) in &fixtures {
            let started = Instant::now();
            let status =
                std::process::Command::new(std::env::current_exe().expect("prebuilt executable"))
                    .args([
                        TEST,
                        "--exact",
                        "--ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(SUBPROCESS_ROOT, root.path())
                    .status()
                    .expect("fresh acceptance process");
            assert!(status.success(), "fresh actor source oracle failed");
            println!(
                "history_acceptance {}",
                serde_json::json!({"phase":"fresh_process_wall","conversations":info.conversations,"sample":ordinal,"elapsed_us":started.elapsed().as_micros()})
            );
        }
    }
}
fn expected(root: &Path) -> Expected {
    let bytes = std::fs::read(root.join("expected.json")).expect("seeded oracle");
    assert!(bytes.len() < 16 * 1024);
    let expected: Expected = serde_json::from_slice(&bytes).expect("oracle schema");
    assert!(SIZES.contains(&expected.conversations));
    assert_eq!(expected.anchors.len(), 3);
    expected
}

async fn sample(root: &Path, expected: &Expected, phase: &str, ordinal: usize, warm: bool) {
    let storage = root.join("state");
    let workspace = root.join("workspace");
    let started = Instant::now();
    let owner = Arc::new(controller(&storage, &workspace));
    let request: dormant_controls::RequestCapture = Arc::default();
    let builds = Arc::new(AtomicUsize::new(0));
    let factory =
        dormant_controls::factory(owner.clone(), &storage, request.clone(), builds.clone());
    let session = SessionId(SESSION.into());
    let child = factory
        .rebind(
            &session,
            Some(&workspace),
            None,
            None,
            &SubagentRecoveryPolicy {
                model_alias: "fast".into(),
                system_prompt: None,
                permission_mode: SessionMode::Plan,
                max_turns: 4,
            },
        )
        .await
        .expect("source-bound dormant rebind")
        .expect("retained child");
    assert_eq!(
        builds.load(Ordering::SeqCst),
        0,
        "discovery must remain inert"
    );
    let discovery_us = started.elapsed().as_micros();
    let selected = Instant::now();
    let controls = child
        .child_controls()
        .await
        .expect("actual selected actor activation");
    let select_us = selected.elapsed().as_micros();
    assert_eq!(
        controls.snapshot.through,
        Some(SequenceId(expected.next_sequence - 1))
    );
    assert_eq!(
        controls.snapshot.controls.pending_plan,
        Some(dormant_controls::artifact())
    );
    assert!(controls.snapshot.controls.questions.is_empty());
    assert!(controls.snapshot.controls.approvals.is_empty());
    let state_started = Instant::now();
    let state = child.child_state().await.expect("actual actor state");
    let state_us = state_started.elapsed().as_micros();
    assert_eq!(state.through, controls.snapshot.through);
    assert_eq!(state.completed_turns, expected.ended_attempts);
    assert_eq!(state.title.as_deref(), Some("Mixed source history"));
    assert!(state.active_turn.is_none() && state.compaction.is_none());
    assert_eq!(state.queued_messages.len(), 1);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    println!(
        "history_acceptance {}",
        serde_json::json!({"phase":phase,"sample":ordinal,"conversations":expected.conversations,"ended_attempts":expected.ended_attempts,"source_events":expected.next_sequence,"source_bytes":expected.journal_bytes,"discovery_us":discovery_us,"select_us":select_us,"state_us":state_us,"controls_bytes":serde_json::to_vec(&controls).expect("controls").len(),"state_bytes":serde_json::to_vec(&state).expect("state").len(),"actor_raw_read_counters":null,"scope":"actual runtime rebind, selected activation and actor queries; seeded filesystem cache is not evicted"})
    );
    if warm {
        for ordinal in 0..WARM_SAMPLES {
            let started = Instant::now();
            let again = child.child_controls().await.expect("same actor controls");
            let again_state = child.child_state().await.expect("same actor state");
            assert_eq!(again, controls);
            assert_eq!(again_state, state);
            println!(
                "history_acceptance {}",
                serde_json::json!({"phase":"same_actor_selected_reads","sample":ordinal,"conversations":expected.conversations,"elapsed_us":started.elapsed().as_micros()})
            );
        }
    }
    transcript(&owner.transcripts, &session, expected, phase, ordinal).await;
    source_probe(&owner.journal_service, expected);
    assert!(
        request.lock().expect("provider calls").is_none(),
        "reattach must not begin inference"
    );
    child.close(None).await.expect("actor effects settled");
    drop(child);
    drop(factory);
    owner
        .journal_service
        .commits
        .shutdown()
        .await
        .expect("source jobs settled");
}

async fn transcript(
    reader: &Arc<crate::transcript_service::TranscriptReader>,
    session: &SessionId,
    expected: &Expected,
    phase: &str,
    ordinal: usize,
) {
    let started = Instant::now();
    let mut catchups = 0;
    let mut returned_bytes = 0;
    for anchor in &expected.anchors {
        let request = TranscriptRead {
            known_view: None,
            position: TranscriptPosition::Around {
                item: TranscriptItemId(anchor.committed),
            },
            max_items: 8,
            max_bytes: 64 * 1024,
        };
        let mut previous_catchup = None;
        let page = loop {
            match reader
                .page(
                    session.clone(),
                    SessionReadScope::Session {},
                    request.clone(),
                )
                .await
                .expect("runtime transcript read")
            {
                TranscriptReadResult::Ready { page } => break page,
                TranscriptReadResult::CatchingUp { through, target } => {
                    assert_eq!(target, Some(SequenceId(expected.next_sequence - 1)));
                    assert!(through.is_none_or(|cut| cut.0 < expected.next_sequence));
                    assert!(
                        through.is_some() && through > previous_catchup,
                        "immutable source catchup must advance its exact prefix"
                    );
                    previous_catchup = through;
                    catchups += 1;
                    assert!(catchups <= expected.next_sequence, "catchup must progress");
                }
                TranscriptReadResult::OrderingChanged { .. } => {
                    panic!("immutable fixture ordering changed")
                }
            }
        };
        assert_eq!(
            page.view.through,
            Some(SequenceId(expected.next_sequence - 1))
        );
        assert_eq!(page.view.digest, expected.digest);
        let item = page
            .items
            .iter()
            .find(|item| item.id.0 == anchor.committed)
            .expect("exact selected source row");
        assert_eq!(
            item.agent_turn.as_ref().map(|turn| turn.0.as_str()),
            Some(anchor.agent_turn.to_string().as_str())
        );
        let TranscriptContent::Conversation {
            role: rw_types::Role::User,
            blocks,
            source,
            ..
        } = &item.content
        else {
            panic!("selected source must be user conversation");
        };
        assert_eq!(source.sequence, anchor.committed);
        let TranscriptConversationBlock::Text { body } = &blocks[0] else {
            panic!("selected input text");
        };
        assert_eq!(body.text, anchor.text);
        let full = reader
            .content(
                session.clone(),
                SessionReadScope::Session {},
                TranscriptContentRead {
                    view: page.view,
                    source: TranscriptContentSource {
                        sequence: anchor.committed,
                        selector: TranscriptContentSelector::ConversationBlock { index: 0 },
                    },
                    offset: 0,
                    max_bytes: 4096,
                },
            )
            .await
            .expect("source-backed full input text");
        assert_eq!(full.text, anchor.text);
        assert!(full.next_offset.is_none());
        returned_bytes += full.text.len();
    }
    println!(
        "history_acceptance {}",
        serde_json::json!({"phase":"runtime_transcript_windows","parent_phase":phase,"sample":ordinal,"conversations":expected.conversations,"elapsed_us":started.elapsed().as_micros(),"catchup_responses":catchups,"anchors":3,"full_text_bytes":returned_bytes,"scope":"first/middle/tail exact source and complete selected text; first sample includes initial semantic projection"})
    );
}
fn source_probe(journals: &Arc<crate::journal_service::JournalService>, expected: &Expected) {
    let source = journals.capture(SESSION).expect("owned source capture");
    assert_eq!(
        source.view.prefix_identity().next_sequence,
        expected.next_sequence
    );
    assert_eq!(source.view.prefix_identity().digest, expected.digest);
    let (page, metrics) = source
        .view
        .page_with_metrics::<EngineEvent>(
            Some(SequenceId(expected.next_sequence - 101)),
            rw_store::session::SessionEventPageLimits {
                max_page_events: 100,
                ..rw_store::session::SessionEventPageLimits::default()
            },
        )
        .expect("physical tail probe");
    assert_eq!(page.events.len(), 100);
    assert_eq!(metrics.records_decoded, 100);
    assert!(metrics.segments_read <= 2);
    println!(
        "history_acceptance {}",
        serde_json::json!({"phase":"separate_source_tail_probe","conversations":expected.conversations,"bytes_read":metrics.bytes_read,"records_scanned":metrics.records_scanned,"records_decoded":metrics.records_decoded,"segments_read":metrics.segments_read,"returned_page_bytes":page.page_bytes,"scope":"diagnostic probe outside actor timing; not actor-internal read counters"})
    );
}
