#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    SESSION_METADATA_VERSION, SessionMetadata, load_session_metadata_any, persist_session_metadata,
};
use serde_json::{Value, json};
use std::path::PathBuf;

fn fixture() -> SessionMetadata {
    SessionMetadata {
        version: SESSION_METADATA_VERSION,
        session_id: "metadata-contract".into(),
        workspace: PathBuf::from("/workspace"),
        model_alias: "default".into(),
        initial_session_context: Vec::new(),
        workspace_generation: 0,
        workspace_roots: vec![PathBuf::from("/workspace")],
        initial_context_workspace_root_count: 1,
        inherited_journal_through: None,
        fork_parent_session_id: None,
        fork_at_turn: None,
        fork_operation_id: None,
    }
}

#[test]
fn metadata_requires_every_schema_field_including_nullable_values() {
    let complete = serde_json::to_value(fixture()).expect("metadata JSON");
    assert!(serde_json::from_value::<SessionMetadata>(complete.clone()).is_ok());
    for field in complete.as_object().expect("object").keys() {
        let mut missing = complete.clone();
        missing.as_object_mut().expect("object").remove(field);
        assert!(
            serde_json::from_value::<SessionMetadata>(missing).is_err(),
            "{field} is required"
        );
    }
    let mut unknown = complete;
    unknown
        .as_object_mut()
        .expect("object")
        .insert("extra".into(), Value::Bool(true));
    assert!(serde_json::from_value::<SessionMetadata>(unknown).is_err());
}

#[test]
fn metadata_rejects_invalid_schema_versions() {
    for version in [0, SESSION_METADATA_VERSION + 1, u16::MAX] {
        let mut value = serde_json::to_value(fixture()).expect("metadata JSON");
        value["version"] = json!(version);
        assert!(serde_json::from_value::<SessionMetadata>(value).is_err());
    }
}

#[test]
fn metadata_reader_rejects_invalid_workspace_mappings_and_identity() {
    let root = tempfile::tempdir().expect("storage");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir_all(root.path().join("sessions/metadata-contract")).expect("session");
    persist_session_metadata(
        root.path(),
        "metadata-contract",
        &workspace,
        "default",
        &[],
        std::slice::from_ref(&workspace),
    )
    .expect("metadata");
    let path = root.path().join("sessions/metadata-contract/metadata.json");
    let valid: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("metadata bytes")).expect("JSON");
    for (field, replacement) in [
        ("workspace_roots", json!([])),
        ("workspace_roots", json!(["relative"])),
        ("initial_context_workspace_root_count", json!(0)),
        ("initial_context_workspace_root_count", json!(2)),
        ("session_id", json!("other-session")),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = replacement;
        std::fs::write(&path, serde_json::to_vec(&invalid).expect("encode"))
            .expect("invalid fixture");
        assert!(
            load_session_metadata_any(root.path(), "metadata-contract").is_err(),
            "reject {field}"
        );
    }
}

#[test]
fn metadata_encoding_rejects_escaped_payloads_before_output_allocation() {
    let mut metadata = fixture();
    metadata.initial_session_context.push(rw_types::Turn {
        role: rw_types::Role::System,
        blocks: vec![rw_types::Block::Text {
            text: "\0".repeat(2 * 1024 * 1024),
        }],
        meta: rw_types::TurnMeta::default(),
    });
    assert!(super::encode_session_metadata(&metadata).is_err());
    metadata.initial_session_context.clear();
    let encoded = super::encode_session_metadata(&metadata).expect("bounded encoding");
    assert_eq!(
        encoded,
        serde_json::to_vec(&metadata).expect("canonical serialization")
    );
    assert_eq!(encoded.capacity(), encoded.len());
}
