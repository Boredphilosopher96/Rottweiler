//! ENOSPC enters the real canonical write and synchronization failure branches.
#![allow(clippy::expect_used)]
use super::{SegmentedJournal, SessionEventPageLimits, SessionStoreError};
use crate::session::journal_io::install_disk_full_fault;
use rw_types::SequenceId;
use serde_json::{Value, json};

fn replay(journal: &SegmentedJournal) -> Vec<Value> {
    let view = journal.read_view();
    view.verify_all().expect("replayed checksums");
    view.page::<Value>(None, SessionEventPageLimits::default())
        .expect("bounded canonical replay")
        .events
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            assert_eq!(event.sequence, SequenceId(index as u64));
            event.event
        })
        .collect()
}

fn recoverable(partial_write_after: Option<usize>, sync_failures: u8) {
    let root = tempfile::tempdir().expect("storage");
    let mut journal = SegmentedJournal::open(root.path(), "disk-full").expect("journal");
    let prefix = json!({"text":"durable prefix"});
    journal
        .append_batch([prefix.clone()])
        .expect("prefix durable");
    let before = journal.read_view().prefix_identity();
    let bytes = std::fs::read(journal.path().join("active.jsonl")).expect("source bytes");
    let fault = install_disk_full_fault(partial_write_after, sync_failures);
    let failure = journal
        .append_batch([json!({"text":"not acknowledged"})])
        .expect_err("disk full cannot acknowledge durability");
    assert!(matches!(failure, SessionStoreError::Io(ref error)
        if error.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error())));
    assert_eq!(journal.next_sequence(), 1);
    assert_eq!(journal.read_view().prefix_identity(), before);
    assert_eq!(
        std::fs::read(journal.path().join("active.jsonl")).expect("rolled back source"),
        bytes
    );
    assert_eq!(replay(&journal), vec![prefix.clone()]);
    drop(fault);
    drop(journal);
    let mut reopened =
        SegmentedJournal::open(root.path(), "disk-full").expect("reopen failed append");
    assert_eq!(reopened.read_view().prefix_identity(), before);
    let next = json!({"text":"retry after capacity recovery"});
    reopened
        .append_batch([next.clone()])
        .expect("durable retry");
    drop(reopened);
    let reopened = SegmentedJournal::open(root.path(), "disk-full").expect("reopen retry");
    assert_eq!(replay(&reopened), vec![prefix, next]);
}

#[test]
fn enospc_before_first_byte_preserves_acknowledged_prefix_and_replay() {
    recoverable(Some(0), 0);
}
#[test]
fn enospc_after_partial_write_rolls_back_before_retry_and_replay() {
    recoverable(Some(7), 0);
}
#[test]
fn enospc_on_sync_removes_complete_unacknowledged_record_before_replay() {
    recoverable(None, 1);
}

#[test]
fn enospc_during_rollback_sync_poison_is_sticky_until_reopen() {
    for (partial_write_after, sync_failures) in [(Some(7), 1), (None, 2)] {
        let root = tempfile::tempdir().expect("storage");
        let mut journal = SegmentedJournal::open(root.path(), "rollback-full").expect("journal");
        let prefix = json!({"text":"durable prefix"});
        journal.append_batch([prefix.clone()]).expect("prefix");
        let before = journal.read_view().prefix_identity();
        let fault = install_disk_full_fault(partial_write_after, sync_failures);
        let failure = journal
            .append_batch([json!({"text":"unproven"})])
            .expect_err("failed proof");
        assert!(
            matches!(failure, SessionStoreError::AppendRollbackFailed { append, rollback }
            if append.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error())
            && rollback.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error()))
        );
        assert_eq!(journal.next_sequence(), 1);
        assert_eq!(journal.read_view().prefix_identity(), before);
        drop(fault);
        assert!(matches!(
            journal.append_batch([json!({"text":"must not write"})]),
            Err(SessionStoreError::EventWriterPoisoned)
        ));
        drop(journal);
        // Live reopening verifies the observed truncated prefix. This is not a
        // claim that an unsuccessful rollback sync survives a power loss.
        let reopened =
            SegmentedJournal::open(root.path(), "rollback-full").expect("validate observed tail");
        assert_eq!(replay(&reopened), vec![prefix]);
        assert_eq!(reopened.read_view().prefix_identity(), before);
    }
}
