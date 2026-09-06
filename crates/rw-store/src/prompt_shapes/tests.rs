#![allow(clippy::expect_used)]
use super::*;

#[test]
fn indexed_shapes_preserve_physical_sources_with_unsigned_turn_identities() {
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("shapes.sqlite3");
    let mut store = PromptShapeStore::open(&path).expect("store");
    for source in 0..256 {
        store
            .record(source, source, b"one shared profile", [7; 32])
            .expect("record");
    }
    store
        .record(u64::MAX, u64::MAX, b"one shared profile", [8; 32])
        .expect("unsigned key");
    assert_eq!(
        store.read(None, None).expect("latest").expect("row").source,
        u64::MAX
    );
    assert_eq!(
        store
            .read(Some(8), Some(8))
            .expect("exact")
            .expect("row")
            .fingerprint,
        [7; 32]
    );
    store
        .record(8, 999, b"different profile", [9; 32])
        .expect("reused turn");
    assert_eq!(
        store
            .read(Some(8), None)
            .expect("latest turn")
            .expect("row")
            .source,
        999
    );
    assert_eq!(
        store
            .read(Some(8), Some(8))
            .expect("pinned")
            .expect("row")
            .source,
        8
    );
    assert!(store.read(Some(9), Some(8)).is_err());
    assert!(store.record(8, 8, b"changed source", [7; 32]).is_err());
    let profiles: usize = store
        .connection
        .query_row("SELECT count(*) FROM profiles", [], |row| row.get(0))
        .expect("deduplicated profiles");
    assert_eq!(
        profiles, 2,
        "failed source substitution rolls its profile back"
    );
    let cache: i32 = store
        .connection
        .pragma_query_value(None, "cache_size", |row| row.get(0))
        .expect("cache allowance");
    assert_eq!(cache, -256);
    let plan: String = store.connection.query_row("EXPLAIN QUERY PLAN SELECT source FROM requests WHERE turn=?1 ORDER BY source DESC LIMIT 1", [8_u64.to_be_bytes().as_slice()], |row| row.get(3)).expect("lookup plan");
    assert!(plan.contains("requests_turn"), "{plan}");
    drop(store);
    let reopened = PromptShapeStore::open(&path).expect("reopen");
    assert_eq!(
        reopened
            .read(Some(8), Some(8))
            .expect("durable source")
            .expect("row")
            .fingerprint,
        [7; 32]
    );
}

#[test]
fn reads_reject_tampered_profiles_and_missing_references() {
    let root = tempfile::tempdir().expect("root");
    let mut store = PromptShapeStore::open(&root.path().join("shapes.sqlite3")).expect("store");
    store.record(1, 3, b"profile", [7; 32]).expect("record");
    store
        .connection
        .execute("UPDATE profiles SET body=?1", [b"tampered".as_slice()])
        .expect("tamper");
    assert!(store.read(Some(1), Some(3)).is_err());
    store
        .connection
        .pragma_update(None, "foreign_keys", false)
        .expect("inject missing reference");
    store
        .connection
        .execute("DELETE FROM profiles", [])
        .expect("remove profile");
    assert!(store.read(Some(1), Some(3)).is_err());
}

#[test]
fn foreign_schema_and_unsafe_storage_are_rejected() {
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("shapes.sqlite3");
    let store = PromptShapeStore::open(&path).expect("store");
    store
        .connection
        .execute_batch("ALTER TABLE requests ADD COLUMN unexpected TEXT")
        .expect("schema drift");
    drop(store);
    assert!(PromptShapeStore::open(&path).is_err());
    #[cfg(unix)]
    {
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");
        assert!(PromptShapeStore::open(&link).is_err());
    }
}
