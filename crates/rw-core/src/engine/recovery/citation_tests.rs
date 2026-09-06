#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery,
    tests::{append, catch_up},
};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::citation_admission::MAX_TURN_CITATIONS;

#[test]
fn canonical_citation_admission_survives_incremental_pages_and_reopen() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut events = vec![PendingEvent::TurnStarted { turn: 1 }];
    events.extend(
        (0..MAX_TURN_CITATIONS).map(|_| PendingEvent::CitationDelta {
            turn: 1,
            uri: "https://example.test".into(),
            title: None,
        }),
    );
    append(&mut journal, events);
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("index");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    drop(recovery);
    append(
        &mut journal,
        vec![PendingEvent::CitationDelta {
            turn: 1,
            uri: "https://overflow.test".into(),
            title: None,
        }],
    );
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("reopen");
    let before = recovery.head().expect("head").next_sequence;
    assert!(recovery.advance(&journal.read_view(), &modes).is_err());
    assert_eq!(
        recovery.head().expect("unchanged head").next_sequence,
        before
    );
}
