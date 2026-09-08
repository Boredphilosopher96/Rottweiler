#![allow(clippy::expect_used)]
use super::*;
use crate::session::{
    SessionIndex, SessionProjection, SessionSummary, journal::JournalPrefixIdentity,
};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("index");
    let projection = SessionProjection {
        summary: SessionSummary {
            id: "many".into(),
            title: "needle".into(),
            updated_unix_ms: 1,
            cost_micros: 0,
            turn_count: 1,
        },
        explicit_title: true,
        complete: true,
        source: JournalPrefixIdentity {
            next_sequence: 4001,
            digest: [0; 32],
        },
        input_claims: vec![1],
    };
    index
        .apply_page(None, &projection, |writer| {
            for sequence in 1..=4000 {
                writer.text(1, rw_types::SequenceId(sequence), 0, "needle common body")?;
            }
            Ok(())
        })
        .expect("posting source");
    root
}

#[test]
fn pre_cancelled_read_never_opens_sqlite_or_checks_the_directory() {
    let control = SessionIndexReadControl::new();
    control.cancel();
    assert!(matches!(
        SessionIndex::search_hits_read_only(
            std::path::Path::new("/absent-search-source"),
            "needle",
            1,
            &control
        ),
        Err(SessionStoreError::IndexReadInterrupted)
    ));
}

#[test]
fn cancellation_inside_common_term_vm_rolls_back_and_allows_the_next_read() {
    let root = fixture();
    let mut control = SessionIndexReadControl::new();
    // The same cancellation flag flips only after the actual SQLite VM has
    // executed multiple progress intervals; no wall-clock scheduling race.
    control.cancel_after_callbacks = Some(3);
    assert!(matches!(
        SessionIndex::search_hits_read_only(root.path(), "needle common", 1, &control),
        Err(SessionStoreError::IndexReadInterrupted)
    ));
    assert!(control.cancelled.load(Ordering::Acquire));
    let rows = SessionIndex::search_hits_read_only(
        root.path(),
        "needle common",
        1,
        &SessionIndexReadControl::new(),
    )
    .expect("connection settled and released");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sequence, Some(rw_types::SequenceId(1)));
}

#[test]
fn expired_deadline_is_sticky_before_statement_start() {
    let control = SessionIndexReadControl {
        deadline: Instant::now(),
        ..SessionIndexReadControl::new()
    };
    let connection = Connection::open_in_memory().expect("sqlite");
    assert!(matches!(
        control.install(&connection),
        Err(SessionStoreError::IndexReadInterrupted)
    ));
}
