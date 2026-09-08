//! Durable integrity and allocation shape proofs for the session payload kernel.
use super::*;
use crate::session::journal::JournalRoot;
use rw_types::session_payload::MAX_PAYLOAD_WINDOW_BYTES;
use std::{
    cell::Cell,
    os::unix::fs::{FileExt as _, MetadataExt as _, PermissionsExt as _},
};

fn store(root: &tempfile::TempDir, session: &str) -> io::Result<SessionPayloadStore> {
    JournalRoot::open(root.path())
        .map_err(io::Error::other)?
        .payloads(session)
        .map_err(io::Error::other)
}

#[test]
fn published_reference_survives_reopen_without_payload_materialization() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let original = store(&root, "parent")?;
    let reference = original.write("durable € receipt\n".as_bytes(), &|| false)?;
    assert!(
        store(&root, "parent").is_err(),
        "a second owner cannot race publication"
    );
    drop(original);
    let reopened = store(&root, "parent")?;
    let result = reopened.window(&reference, 0, None, &|| false)?;
    assert_eq!(result.content, "durable € receipt\n");
    assert!(!result.has_more);
    Ok(())
}

#[test]
fn query_dense_newlines_and_large_matching_line_remain_bounded() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let owner = store(&root, "session")?;
    let dense = "x\n".repeat(2 * 1024 * 1024);
    let reference = owner.write(dense.as_bytes(), &|| false)?;
    let first = owner.window(&reference, 0, Some("x"), &|| false)?;
    assert!(first.has_more);
    assert!(!first.line_truncated);
    assert!(first.content.capacity() <= MAX_PAYLOAD_WINDOW_BYTES);
    assert!(first.next_offset <= MAX_PAYLOAD_WINDOW_BYTES + 2);
    assert_eq!(
        first.content.lines().count(),
        MAX_PAYLOAD_WINDOW_BYTES.div_ceil(2)
    );
    let giant = format!("needle{}\nlast needle", "€".repeat(1024 * 1024));
    let reference = owner.write(giant.as_bytes(), &|| false)?;
    let first = owner.window(&reference, 0, Some("needle"), &|| false)?;
    assert!(first.line_truncated);
    assert!(first.has_more);
    assert!(first.content.capacity() <= MAX_PAYLOAD_WINDOW_BYTES);
    assert!(first.content.ends_with('€'));
    let second = owner.window(&reference, first.next_offset, Some("needle"), &|| false)?;
    assert_eq!(second.content, "last needle");
    assert!(!second.has_more);
    Ok(())
}

#[test]
fn raw_utf8_pagination_is_lossless_across_chunk_and_output_boundaries() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let owner = store(&root, "session")?;
    let body = format!("a{}z", "🦀".repeat(180_000));
    let reference = owner.write(body.as_bytes(), &|| false)?;
    let mut offset = 0;
    while offset < body.len() {
        let window = owner.window(&reference, offset, None, &|| false)?;
        assert_eq!(window.content, body[offset..window.next_offset]);
        assert!(window.next_offset > offset);
        assert!(window.content.capacity() <= MAX_PAYLOAD_WINDOW_BYTES);
        offset = window.next_offset;
    }
    assert!(owner.window(&reference, 2, None, &|| false).is_err());
    Ok(())
}

#[test]
fn manifest_rewrite_cannot_authorize_changed_chunk_bytes() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let owner = store(&root, "session")?;
    let reference = owner.write(b"original", &|| false)?;
    let path = root
        .path()
        .join("sessions/session/payloads")
        .join(format!("{}.payload", reference.digest));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let file = std::fs::OpenOptions::new().write(true).open(&path)?;
    file.write_all_at(b"modified", 48)?;
    file.write_all_at(blake3::hash(b"modified").as_bytes(), 16)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))?;
    let error = owner
        .window(&reference, 0, None, &|| false)
        .err()
        .ok_or_else(|| io::Error::other("retained manifest accepted replacement"))?;
    assert!(error.to_string().contains("identity mismatch"));
    drop(owner);
    assert!(
        store(&root, "session").is_err(),
        "reopen also verifies manifest against file identity"
    );
    Ok(())
}

#[test]
fn unchanged_manifest_rejects_body_corruption_and_forged_length() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let owner = store(&root, "session")?;
    let reference = owner.write(b"original", &|| false)?;
    let path = root
        .path()
        .join("sessions/session/payloads")
        .join(format!("{}.payload", reference.digest));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let file = std::fs::OpenOptions::new().write(true).open(&path)?;
    file.write_all_at(b"modified", 48)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))?;
    assert!(owner.window(&reference, 0, None, &|| false).is_err());
    let mut forged = reference;
    forged.bytes += 1;
    assert!(owner.window(&forged, 0, None, &|| false).is_err());
    Ok(())
}

#[test]
fn selected_fork_payloads_are_independent_files_and_survive_parent_deletion() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let parent = store(&root, "parent")?;
    let first = parent.write(b"included in fork prefix", &|| false)?;
    let later = parent.write(b"after fork cutoff", &|| false)?;
    let child = store(&root, "child")?;
    parent.copy_to(&first, &child, &|| false)?;
    assert!(child.window(&later, 0, None, &|| false).is_err());
    let payload = format!("{}.payload", first.digest);
    let parent_meta =
        std::fs::metadata(root.path().join("sessions/parent/payloads").join(&payload))?;
    let child_meta = std::fs::metadata(root.path().join("sessions/child/payloads").join(&payload))?;
    assert_ne!(parent_meta.ino(), child_meta.ino());
    assert_eq!(parent_meta.nlink(), 1);
    assert_eq!(child_meta.nlink(), 1);
    drop(parent);
    drop(child);
    std::fs::remove_dir_all(root.path().join("sessions/parent"))?;
    assert_eq!(
        store(&root, "child")?
            .window(&first, 0, None, &|| false)?
            .content,
        "included in fork prefix"
    );
    Ok(())
}

#[test]
fn quota_refuses_new_objects_without_evicting_old_references() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let owner = store(&root, "session")?;
    let first = owner.write(b"0", &|| false)?;
    for index in 1..MAX_SESSION_PAYLOADS {
        owner.write(index.to_string().as_bytes(), &|| false)?;
    }
    assert!(owner.write(b"one more", &|| false).is_err());
    assert_eq!(owner.window(&first, 0, None, &|| false)?.content, "0");
    drop(owner);
    assert_eq!(
        store(&root, "session")?
            .window(&first, 0, None, &|| false)?
            .content,
        "0"
    );
    let mut records = BTreeMap::new();
    records.insert("one".into(), MAX_SESSION_PAYLOAD_TOTAL_BYTES);
    assert!(admit(&records, 1).is_err());
    Ok(())
}

#[test]
fn cancelled_query_stops_without_retaining_scan_or_matching_line_buffers() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let owner = store(&root, "session")?;
    let reference = owner.write(&vec![b'a'; 2 * 1024 * 1024], &|| false)?;
    let calls = Cell::new(0);
    let error = owner
        .window(&reference, 0, Some("missing"), &|| {
            calls.set(calls.get() + 1);
            calls.get() > 3
        })
        .err()
        .ok_or_else(|| io::Error::other("scan ignored cancellation"))?;
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(calls.get(), 4);
    assert!(owner.window(&reference, 0, None, &|| false).is_ok());
    Ok(())
}
