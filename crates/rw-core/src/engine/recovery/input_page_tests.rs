#![allow(clippy::expect_used)]
use super::*;
use crate::engine::{
    PendingEvent,
    recovery::tests::{append, terminal},
};
use rw_store::session::{SessionEventPageLimits, journal::SegmentedJournal};
use rw_types::{SequenceId, conversation_input::InputSelection};

fn accepted(text: &str) -> PendingEvent {
    PendingEvent::UserMessageAccepted {
        turn: 1,
        content: text.into(),
        attachments: vec![],
    }
}
fn commit(turn: u64) -> PendingEvent {
    PendingEvent::ConversationInputCommitted {
        agent_turn: turn,
        accepted_source: SequenceId(1),
        selection: InputSelection::Accepted {},
    }
}
fn checkpoint(source: &JournalReadView, through: u64) -> InputClaimCheckpoint {
    let prefix = source
        .prefix_through(Some(SequenceId(through)))
        .expect("prefix");
    let page = prefix
        .verified_page::<EngineEvent>(None, SessionEventPageLimits::default())
        .expect("page");
    let mut claims =
        InputClaimPage::new(&page, InputClaimCheckpoint::default()).expect("initial claim state");
    while claims.next_event().expect("claim").is_some() {}
    claims.checkpoint().expect("checkpoint")
}

#[test]
fn input_checkpoint_rejects_another_digest_and_partial_watermark() {
    let left = tempfile::tempdir().expect("left");
    let right = tempfile::tempdir().expect("right");
    let mut first = SegmentedJournal::open(left.path(), "canonical").expect("first");
    let mut second = SegmentedJournal::open(right.path(), "canonical").expect("second");
    append(
        &mut first,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            accepted("first"),
            commit(1),
        ],
    );
    append(
        &mut second,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            accepted("different"),
            commit(1),
        ],
    );
    let first = first.read_view();
    let second = second.read_view();
    let checkpoint = checkpoint(&first, 1);
    let page = second
        .verified_page::<EngineEvent>(Some(SequenceId(1)), SessionEventPageLimits::default())
        .expect("other source page");
    assert!(InputClaimPage::new(&page, checkpoint.clone()).is_err());
    let page = first
        .verified_page::<EngineEvent>(Some(SequenceId(0)), SessionEventPageLimits::default())
        .expect("overlapping page");
    assert!(InputClaimPage::new(&page, checkpoint.clone()).is_err());
    let bytes = checkpoint.encode().expect("encode");
    assert!(
        InputClaimCheckpoint::decode(
            &bytes,
            second
                .prefix_through(Some(SequenceId(1)))
                .expect("other prefix")
                .prefix_identity()
        )
        .is_err()
    );
    let page = first
        .verified_page::<EngineEvent>(Some(SequenceId(1)), SessionEventPageLimits::default())
        .expect("matching page");
    let mut claims = InputClaimPage::new(&page, checkpoint).expect("matching claim");
    let resolved = claims
        .next_event()
        .expect("transition")
        .expect("commit")
        .materialize()
        .expect("materialized");
    assert!(
        matches!(&*resolved, EngineEvent::ConversationTurnCommitted { turn, .. } if turn.blocks == vec![rw_types::Block::Text { text: "first".into() }])
    );
}

#[test]
fn published_content_cannot_bypass_a_later_invalid_cross_turn_claim() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            accepted("private pending input"),
        ],
    );
    let source = journal.read_view();
    let mut transcript = crate::transcript::TranscriptProjector::open(&source).expect("transcript");
    while transcript
        .advance(&source)
        .expect("initial prefix")
        .has_more
    {}
    append(
        &mut journal,
        vec![
            terminal(1),
            PendingEvent::TurnStarted { turn: 2 },
            commit(2),
        ],
    );
    let source = journal.read_view();
    assert!(transcript.advance(&source).is_err());
    assert_eq!(
        transcript
            .index()
            .head()
            .expect("published head")
            .prefix
            .next_sequence,
        2
    );
    assert!(
        crate::transcript::TranscriptProjector::materialize_source(
            transcript.index(),
            &source,
            SequenceId(4)
        )
        .is_err()
    );
}

#[test]
fn input_checkpoint_requires_bounded_complete_state() {
    let checkpoint = InputClaimCheckpoint::default();
    let encoded = checkpoint.encode().expect("empty checkpoint");
    let mut missing: serde_json::Value = serde_json::from_slice(&encoded).expect("JSON");
    missing.as_object_mut().expect("object").remove("claims");
    assert!(
        InputClaimCheckpoint::decode(
            &serde_json::to_vec(&missing).expect("JSON"),
            JournalPrefixIdentity::empty()
        )
        .is_err()
    );
    let pending = serde_json::json!({"agent_turn":1,"claimed_turn":1,"sequence":0,"retained":false,"ended":false});
    let oversized = serde_json::json!({"session":"canonical","next_sequence":1,"active":1,"finished":[],"pending":vec![pending; rw_types::session_state::MAX_SESSION_QUEUE_ITEMS + 1]});
    assert!(serde_json::from_value::<InputClaimState>(oversized).is_err());
}

#[test]
fn maximum_pending_input_checkpoint_round_trips_within_its_source_profile() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        (0..rw_types::session_state::MAX_SESSION_QUEUE_ITEMS)
            .map(|_| accepted("pending"))
            .collect(),
    );
    let source = journal.read_view();
    let state = checkpoint(
        &source,
        rw_types::session_state::MAX_SESSION_QUEUE_ITEMS as u64 - 1,
    );
    let encoded = state.encode().expect("bounded encode");
    let reopened = InputClaimCheckpoint::decode(&encoded, source.prefix_identity())
        .expect("maximal legal metadata profile");
    assert_eq!(
        reopened.claims.pending().len(),
        rw_types::session_state::MAX_SESSION_QUEUE_ITEMS
    );
}
