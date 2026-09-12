#![allow(clippy::expect_used)]
use super::*;
use rw_store::session::journal::SegmentedJournal;

#[test]
fn one_lazy_writer_is_shared_across_sessions_and_rejected_foreign_sources_cannot_initialize_it() {
    let root = tempfile::tempdir().expect("root");
    let service = JournalService::new(root.path()).expect("service");
    assert!(
        !root.path().join("index.sqlite").exists(),
        "read-service startup must not initialize SQLite"
    );
    let first = SegmentedJournal::open(root.path(), "first").expect("first journal");
    let second = SegmentedJournal::open(root.path(), "second").expect("second journal");
    let foreign_root = tempfile::tempdir().expect("foreign root");
    let foreign = SegmentedJournal::open(foreign_root.path(), "first").expect("foreign journal");
    assert!(service.search_index("first", &foreign.read_view()).is_err());
    assert!(!root.path().join("index.sqlite").exists());
    let first_index = service
        .search_index("first", &first.read_view())
        .expect("first writer");
    let second_index = service
        .search_index("second", &second.read_view())
        .expect("shared writer");
    assert!(Arc::ptr_eq(&first_index, &second_index));
    drop(first_index);
    let reacquired = service
        .search_index("first", &first.read_view())
        .expect("retained owner");
    assert!(Arc::ptr_eq(&reacquired, &second_index));
}

#[test]
fn failed_initialization_is_not_cached_and_can_retry_after_explicit_repair() {
    let root = tempfile::tempdir().expect("root");
    let service = JournalService::new(root.path()).expect("service");
    let journal = SegmentedJournal::open(root.path(), "retry").expect("journal");
    let path = root.path().join("index.sqlite");
    std::fs::write(&path, b"not a SQLite database").expect("corrupt fixture");
    assert!(service.search_index("retry", &journal.read_view()).is_err());
    assert!(service.search_index.writer.lock().expect("owner").is_none());
    std::fs::remove_file(path).expect("explicit fixture repair");
    let index = service
        .search_index("retry", &journal.read_view())
        .expect("retry writer");
    assert!(index.list(1).expect("valid empty index").is_empty());
}
