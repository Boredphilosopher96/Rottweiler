#![allow(clippy::expect_used)]

use super::*;
use crate::session::journal::SegmentedJournal;
use tempfile::tempdir;

fn key(ordinal: u64) -> RecoveryKey {
    RecoveryKey {
        namespace: 1,
        scope: 2,
        ordinal,
    }
}
fn put(ordinal: u64, payload: &[u8]) -> RecoveryMutation {
    RecoveryMutation::Put(RecoveryRow {
        key: key(ordinal),
        payload: payload.to_vec(),
    })
}

#[test]
fn consistent_snapshot_retains_prefix_rows_and_lock_across_commit_and_owner_drop() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "snapshot").expect("journal");
    let mut index = RecoveryIndex::open(
        &journal.read_view(),
        crate::session::recovery_index::RecoveryProjection::Conversation,
        1,
    )
    .expect("index");
    journal.append_batch([1_u64]).expect("append");
    let first = journal
        .read_view()
        .prove_advance(JournalPrefixIdentity::empty())
        .expect("advance");
    index
        .apply(&first, b"one", &[put(0, b"old")], &[])
        .expect("apply");
    let old = index.read().expect("old snapshot");
    journal.append_batch([2_u64]).expect("append");
    let second = journal
        .read_view()
        .prove_advance(first.next().prefix_identity())
        .expect("advance");
    index
        .apply(&second, b"two", &[put(0, b"new"), put(1, b"added")], &[])
        .expect("apply");
    assert_eq!(old.head().checkpoint, b"one");
    assert_eq!(
        old.get(key(0)).expect("old row").expect("exists").payload,
        b"old"
    );
    assert!(old.get(key(1)).expect("old absent").is_none());
    assert_eq!(
        old.bind_source(&journal.read_view())
            .expect("source")
            .prefix_identity(),
        first.next().prefix_identity()
    );
    let new = index.read().expect("new snapshot");
    assert_eq!(new.head().checkpoint, b"two");
    assert_eq!(
        new.get(key(0)).expect("new row").expect("exists").payload,
        b"new"
    );
    drop(index);
    assert!(matches!(
        RecoveryIndex::open(
            &journal.read_view(),
            crate::session::recovery_index::RecoveryProjection::Conversation,
            1
        ),
        Err(RecoveryIndexError::Busy)
    ));
    drop(new);
    assert!(matches!(
        RecoveryIndex::rebuild(
            &journal.read_view(),
            crate::session::recovery_index::RecoveryProjection::Conversation,
            1
        ),
        Err(RecoveryIndexError::Busy)
    ));
    drop(old);
    let reopened = RecoveryIndex::open(
        &journal.read_view(),
        crate::session::recovery_index::RecoveryProjection::Conversation,
        1,
    )
    .expect("reopen");
    assert_eq!(
        reopened.head().expect("head").prefix,
        second.next().prefix_identity()
    );
}

#[test]
fn stale_and_foreign_transitions_preserve_rows_and_checkpoint() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "mine").expect("journal");
    let mut foreign = SegmentedJournal::open(root.path(), "foreign").expect("foreign");
    let mut index = RecoveryIndex::open(
        &journal.read_view(),
        crate::session::recovery_index::RecoveryProjection::Conversation,
        1,
    )
    .expect("index");
    let empty = index.read().expect("empty");
    assert!(matches!(
        empty.bind_source(&foreign.read_view()),
        Err(RecoveryIndexError::Invalid("foreign journal"))
    ));
    journal.append_batch([1_u64]).expect("append");
    foreign.append_batch([1_u64]).expect("append");
    let advance = journal
        .read_view()
        .prove_advance(JournalPrefixIdentity::empty())
        .expect("advance");
    let foreign_advance = foreign
        .read_view()
        .prove_advance(JournalPrefixIdentity::empty())
        .expect("foreign advance");
    assert_eq!(
        advance.next().prefix_identity(),
        foreign_advance.next().prefix_identity()
    );
    assert!(matches!(
        index.apply(&foreign_advance, b"wrong", &[put(0, b"wrong")], &[]),
        Err(RecoveryIndexError::Invalid("foreign journal"))
    ));
    index
        .apply(&advance, b"right", &[put(0, b"right")], &[])
        .expect("apply");
    assert!(matches!(
        index.apply(&advance, b"stale", &[put(0, b"stale")], &[]),
        Err(RecoveryIndexError::Stale)
    ));
    let read = index.read().expect("snapshot");
    assert_eq!(read.head().checkpoint, b"right");
    assert_eq!(
        read.get(key(0)).expect("row").expect("exists").payload,
        b"right"
    );
}

#[test]
fn byte_and_row_admission_precedes_storage_mutation() {
    let root = tempdir().expect("root");
    let journal = SegmentedJournal::open(root.path(), "limits").expect("journal");
    let mut index = RecoveryIndex::open(
        &journal.read_view(),
        crate::session::recovery_index::RecoveryProjection::Conversation,
        1,
    )
    .expect("index");
    let advance = journal
        .read_view()
        .prove_advance(JournalPrefixIdentity::empty())
        .expect("advance");
    let before = index.io_metrics();
    assert!(matches!(
        index.apply(&advance, &vec![0; MAX_RECOVERY_HEAD_BYTES + 1], &[], &[]),
        Err(RecoveryIndexError::Limit(_))
    ));
    assert!(matches!(
        index.apply(
            &advance,
            b"",
            &[put(0, &vec![0; MAX_RECOVERY_ROW_BYTES + 1])],
            &[]
        ),
        Err(RecoveryIndexError::Limit(_))
    ));
    let too_many = vec![put(0, b""); MAX_RECOVERY_BATCH_ROWS + 1];
    assert!(matches!(
        index.apply(&advance, b"", &too_many, &[]),
        Err(RecoveryIndexError::Limit(_))
    ));
    let too_large: Vec<_> = (0..16)
        .map(|i| put(i, &vec![0; MAX_RECOVERY_ROW_BYTES]))
        .collect();
    assert!(matches!(
        index.apply(&advance, b"", &too_large, &[]),
        Err(RecoveryIndexError::Limit(_))
    ));
    let after = index.io_metrics();
    assert_eq!(after.bytes_written, before.bytes_written);
    assert_eq!(after.syncs, before.syncs);
    assert!(
        index
            .read()
            .expect("read")
            .get(key(0))
            .expect("row")
            .is_none()
    );
}

#[test]
fn pages_seek_by_key_and_report_truthful_byte_boundaries() {
    let root = tempdir().expect("root");
    let journal = SegmentedJournal::open(root.path(), "pages").expect("journal");
    let mut index = RecoveryIndex::open(
        &journal.read_view(),
        crate::session::recovery_index::RecoveryProjection::Conversation,
        1,
    )
    .expect("index");
    let advance = journal
        .read_view()
        .prove_advance(JournalPrefixIdentity::empty())
        .expect("advance");
    index
        .apply(
            &advance,
            b"",
            &(0..10).map(|i| put(i * 100, b"four")).collect::<Vec<_>>(),
            &[],
        )
        .expect("apply");
    let read = index.read().expect("read");
    let page = read.page(1, 2, None, 10, 56).expect("page");
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.next_cursor, Some(100));
    assert_eq!(page.retained_bytes, 56);
    assert!(page.has_more);
    let middle = read.page(1, 2, Some(350), 2, 1000).expect("seek");
    assert_eq!(middle.rows[0].key.ordinal, 400);
    assert_eq!(middle.next_cursor, Some(500));
    assert!(middle.has_more);
    let tail = read.page(1, 2, Some(800), 2, 1000).expect("tail");
    assert_eq!(tail.next_cursor, Some(900));
    assert!(!tail.has_more);
    let empty = read.page(1, 3, None, 2, 1000).expect("other scope");
    assert!(empty.rows.is_empty());
    assert!(matches!(
        read.page(1, 2, None, 2, 27),
        Err(RecoveryIndexError::Limit("page cannot fit row"))
    ));
    assert!(matches!(
        read.page(1, 2, Some(u64::MAX), 2, 1000),
        Err(RecoveryIndexError::Limit("cursor overflow"))
    ));
}

#[test]
fn incompatible_schema_requires_explicit_derived_rebuild_without_raw_changes() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "version").expect("journal");
    journal.append_batch([1_u64, 2]).expect("append");
    let view = journal.read_view();
    let mut index = RecoveryIndex::open(
        &view,
        crate::session::recovery_index::RecoveryProjection::Conversation,
        1,
    )
    .expect("index");
    index
        .apply(
            &view
                .prove_advance(JournalPrefixIdentity::empty())
                .expect("advance"),
            b"state",
            &[put(0, b"row")],
            &[],
        )
        .expect("apply");
    drop(index);
    assert!(matches!(
        RecoveryIndex::open(
            &view,
            crate::session::recovery_index::RecoveryProjection::Conversation,
            2
        ),
        Err(RecoveryIndexError::Invalid("projection version"))
    ));
    let rebuilt = RecoveryIndex::rebuild(
        &view,
        crate::session::recovery_index::RecoveryProjection::Conversation,
        2,
    )
    .expect("explicit rebuild");
    assert_eq!(
        rebuilt.head().expect("head").prefix,
        JournalPrefixIdentity::empty()
    );
    assert_eq!(rebuilt.head().expect("head").version, 2);
    assert_eq!(
        journal.read_view().prefix_identity(),
        view.prefix_identity()
    );
    assert_eq!(
        view.page::<u64>(None, crate::session::SessionEventPageLimits::default())
            .expect("raw")
            .events
            .len(),
        2
    );
}

#[test]
fn identity_lookups_publish_atomically_and_reject_oversized_keys_before_mutation() {
    use super::{MAX_RECOVERY_LOOKUP_KEY_BYTES, RecoveryLookup};
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "lookups").expect("journal");
    journal.append_batch([1_u64]).expect("source");
    let view = journal.read_view();
    let mut index = RecoveryIndex::open(
        &view,
        crate::session::recovery_index::RecoveryProjection::Conversation,
        1,
    )
    .expect("index");
    let advance = view
        .prove_advance(JournalPrefixIdentity::empty())
        .expect("proof");
    let before = index.read().expect("prior view");
    assert!(
        index
            .apply(
                &advance,
                b"invalid",
                &[],
                &[RecoveryLookup {
                    namespace: 1,
                    key: vec![0; MAX_RECOVERY_LOOKUP_KEY_BYTES + 1],
                    payload: b"source".to_vec()
                }]
            )
            .is_err()
    );
    assert_eq!(
        index.head().expect("head").prefix,
        JournalPrefixIdentity::empty()
    );
    index
        .apply(
            &advance,
            b"ready",
            &[],
            &[RecoveryLookup {
                namespace: 1,
                key: b"call:attempt".to_vec(),
                payload: b"source".to_vec(),
            }],
        )
        .expect("publish");
    assert_eq!(before.lookup(1, b"call:attempt").expect("old lookup"), None);
    assert_eq!(
        index
            .read()
            .expect("view")
            .lookup(1, b"call:attempt")
            .expect("lookup"),
        Some(b"source".to_vec())
    );
}
