#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    SESSION_METADATA_VERSION, SessionMetadata, load_session_metadata_any, persist_session_metadata,
};
use serde_json::{Value, json};
use std::path::PathBuf;

struct ReadAllowance {
    limit: usize,
    bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl rw_core::recovery::HistoryWorkingAllowance for ReadAllowance {
    fn resize(&mut self, bytes: usize) -> Result<(), rw_core::AgentLoopError> {
        if bytes > self.limit {
            return Err(rw_core::AgentLoopError::Persistence(
                "fixture metadata allocation denied".into(),
            ));
        }
        self.bytes.store(bytes, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
impl Drop for ReadAllowance {
    fn drop(&mut self) {
        self.bytes.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn metadata_read_admits_raw_and_typed_storage_and_retains_it_with_the_result() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let root = tempfile::tempdir().expect("root");
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
        crate::CanonicalReadBudget::new().reserve(),
    )
    .expect("fixture");
    let bytes = Arc::new(AtomicUsize::new(0));
    let denied = load_session_metadata_any(
        root.path(),
        "metadata-contract",
        Box::new(ReadAllowance {
            limit: 0,
            bytes: Arc::clone(&bytes),
        }),
    );
    assert!(
        denied
            .expect_err("raw admission denied")
            .to_string()
            .contains("allocation denied")
    );
    assert_eq!(bytes.load(Ordering::SeqCst), 0);
    let owned = load_session_metadata_any(
        root.path(),
        "metadata-contract",
        Box::new(ReadAllowance {
            limit: 1024 * 1024,
            bytes: Arc::clone(&bytes),
        }),
    )
    .expect("owned decode");
    assert_eq!(owned.session_id, "metadata-contract");
    assert!(bytes.load(Ordering::SeqCst) > 0);
    drop(owned);
    assert_eq!(bytes.load(Ordering::SeqCst), 0);

    let mut dense = fixture();
    dense.workspace.clone_from(&workspace);
    dense.workspace_roots = vec![workspace];
    dense.initial_session_context = vec![rw_core::base_agent_system_turn(); 200];
    let path = root.path().join("sessions/metadata-contract/metadata.json");
    let encoded = serde_json::to_vec(&dense).expect("dense");
    let raw_peak = encoded.len() * 2;
    std::fs::write(path, encoded).expect("replace fixture");
    let rejected = load_session_metadata_any(
        root.path(),
        "metadata-contract",
        Box::new(ReadAllowance {
            limit: raw_peak,
            bytes: Arc::clone(&bytes),
        }),
    );
    assert!(
        rejected
            .expect_err("typed graph must also be admitted")
            .to_string()
            .contains("allocation denied")
    );
    assert_eq!(bytes.load(Ordering::SeqCst), 0);
}

fn fixture() -> SessionMetadata {
    SessionMetadata {
        budget_session_id: rw_types::SessionId("fixture".into()),
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
        crate::CanonicalReadBudget::new().reserve(),
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
        ("budget_session_id", json!("../outside")),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = replacement;
        std::fs::write(&path, serde_json::to_vec(&invalid).expect("encode"))
            .expect("invalid fixture");
        assert!(
            load_session_metadata_any(
                root.path(),
                "metadata-contract",
                crate::CanonicalReadBudget::new().reserve(),
            )
            .is_err(),
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

#[test]
fn metadata_loader_requires_nullable_fields_in_initial_context() {
    use rw_types::{Block, Role, Turn, TurnMeta};
    let root = tempfile::tempdir().expect("storage");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir_all(root.path().join("sessions/context-schema"))
        .expect("session directory");
    let context = [Turn {
        role: Role::System,
        blocks: vec![
            Block::Thinking {
                content: "context".into(),
                signature: None,
            },
            Block::Citation {
                uri: "https://example.org/source".into(),
                title: None,
                excerpt: None,
            },
        ],
        meta: TurnMeta::default(),
    }];
    persist_session_metadata(
        root.path(),
        "context-schema",
        &workspace,
        "default",
        &context,
        std::slice::from_ref(&workspace),
        crate::CanonicalReadBudget::new().reserve(),
    )
    .expect("producer metadata");
    let path = root.path().join("sessions/context-schema/metadata.json");
    let bytes = std::fs::read(&path).expect("metadata bytes");
    let complete: Value = serde_json::from_slice(&bytes).expect("metadata JSON");
    assert_eq!(
        load_session_metadata_any(
            root.path(),
            "context-schema",
            crate::CanonicalReadBudget::new().reserve(),
        )
        .expect("explicit nulls are valid")
        .initial_session_context,
        context
    );
    for (pointer, field) in [
        ("/initial_session_context/0/meta", "created_at"),
        ("/initial_session_context/0/meta", "model"),
        ("/initial_session_context/0/blocks/0", "signature"),
        ("/initial_session_context/0/blocks/1", "title"),
        ("/initial_session_context/0/blocks/1", "excerpt"),
    ] {
        let mut missing = complete.clone();
        let removed = missing
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .expect("IR object")
            .remove(field);
        assert_eq!(removed, Some(Value::Null));
        std::fs::write(
            &path,
            serde_json::to_vec(&missing).expect("modified metadata"),
        )
        .expect("persist one omission");
        let error = load_session_metadata_any(
            root.path(),
            "context-schema",
            crate::CanonicalReadBudget::new().reserve(),
        )
        .expect_err("missing internal IR field");
        assert!(
            error
                .to_string()
                .contains(&format!("missing field `{field}`")),
            "{error}"
        );
    }
    std::fs::write(&path, bytes).expect("restore producer bytes");
    assert_eq!(
        load_session_metadata_any(
            root.path(),
            "context-schema",
            crate::CanonicalReadBudget::new().reserve(),
        )
        .expect("restored metadata")
        .initial_session_context,
        context
    );
}
