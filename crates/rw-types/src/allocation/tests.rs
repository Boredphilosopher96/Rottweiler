#![cfg(test)]
#![allow(clippy::expect_used)]
use super::*;
use crate::{EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, TurnId};

fn meta() -> EventMeta {
    EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("allocation".into()),
        sequence_id: SequenceId(0),
        emitted_at: "2026-09-05T00:00:00Z".into(),
        caused_by: None,
    }
}

#[test]
fn short_event_payload_retains_and_charges_large_unused_string_capacity() {
    let mut text = String::with_capacity(8 * 1024 * 1024);
    text.push('x');
    let event = EngineEvent::TextDelta {
        meta: meta(),
        turn_id: TurnId("1".into()),
        text,
    };
    assert!(serde_json::to_vec(&event).is_ok_and(|bytes| bytes.len() < 1024));
    assert!(
        event
            .prepared_heap_bytes()
            .is_some_and(|bytes| bytes >= 8 * 1024 * 1024)
    );
}

#[test]
fn nested_json_and_empty_reserved_vectors_keep_capacity_in_the_charge() {
    let mut entries = Vec::<serde_json::Value>::with_capacity(8192);
    let capacity = entries.capacity() * size_of::<serde_json::Value>();
    assert_eq!(entries.prepared_heap_bytes(), Some(capacity));
    entries.push(serde_json::Value::String(String::with_capacity(
        1024 * 1024,
    )));
    let mut map = serde_json::Map::new();
    map.insert("nested".into(), serde_json::Value::Array(entries));
    assert!(
        serde_json::Value::Object(map)
            .prepared_heap_bytes()
            .is_some_and(|bytes| bytes >= capacity + 1024 * 1024)
    );
}

#[test]
fn validated_foreign_progress_still_charges_unused_capacity() {
    let mut text = String::with_capacity(1024 * 1024);
    text.push('x');
    let progress = rw_operation_contract::ToolProgress::new(text, None);
    assert!(progress.is_ok_and(|value| value.prepared_heap_bytes() == Some(1024 * 1024)));
}

#[derive(rw_memory_derive::PrepareAllocation)]
struct Named {
    value: String,
    more: Option<Vec<String>>,
}
#[derive(rw_memory_derive::PrepareAllocation)]
struct Tuple(String);
#[derive(rw_memory_derive::PrepareAllocation)]
enum Variants {
    Empty,
    Tuple(Tuple),
    Named { value: Named },
}

#[test]
fn derive_covers_struct_tuple_enum_and_nested_optional_allocations() {
    let value = Variants::Named {
        value: Named {
            value: String::with_capacity(16),
            more: Some(vec![String::with_capacity(32)]),
        },
    };
    assert_eq!(
        value.prepared_heap_bytes(),
        Some(16 + size_of::<String>() + 32)
    );
    assert_eq!(
        Variants::Tuple(Tuple(String::with_capacity(64))).prepared_heap_bytes(),
        Some(64)
    );
    assert_eq!(Variants::Empty.prepared_heap_bytes(), Some(0));
}

struct Overflow;
impl PrepareAllocation for Overflow {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        Some(usize::MAX)
    }
    fn prepare_allocations(&mut self) {}
}
#[test]
fn overflow_is_rejected_instead_of_wrapping_the_admission_charge() {
    assert_eq!(vec![Overflow, Overflow].prepared_heap_bytes(), None);
}

#[test]
fn btree_node_model_is_reviewed_with_the_pinned_rust_toolchain() {
    assert!(include_str!("../../../../rust-toolchain.toml").contains("1.97.1"));
}

#[test]
fn preparation_preserves_json_insertion_order_and_nested_values() {
    let mut nested = serde_json::Map::with_capacity(16_384);
    nested.insert("z".into(), serde_json::Value::String("first".into()));
    nested.insert("a".into(), serde_json::Value::String("second".into()));
    let mut map = serde_json::Map::with_capacity(32_768);
    map.insert("nested".into(), serde_json::Value::Object(nested));
    map.insert("last".into(), serde_json::Value::Bool(true));
    let value = serde_json::Value::Object(map);
    let before = serde_json::to_string(&value).expect("serialize original");
    let plan = AllocationPlan::new(value).expect("preflight");
    let bytes = plan.bytes();
    let prepared = plan.prepare();
    assert_eq!(prepared.bytes(), bytes);
    assert_eq!(prepared.value().prepared_bytes(), Some(bytes));
    assert_eq!(
        serde_json::to_string(&prepared).expect("serialize prepared"),
        before
    );
    assert!(
        before.find("first").expect("first entry") < before.find("second").expect("second entry")
    );
}

#[test]
fn preflight_rejects_unsupported_json_depth_before_preparation() {
    let mut value = serde_json::Value::Null;
    for _ in 0..=MAX_JSON_DEPTH {
        value = serde_json::Value::Array(vec![value]);
    }
    assert!(AllocationPlan::new(value).is_err());
}

#[test]
fn preflight_does_not_normalize_until_the_caller_admits_it() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PREPARED: AtomicBool = AtomicBool::new(false);
    struct Probe;
    impl PrepareAllocation for Probe {
        fn prepared_heap_bytes(&self) -> Option<usize> {
            Some(1024)
        }
        fn prepare_allocations(&mut self) {
            PREPARED.store(true, Ordering::SeqCst);
        }
    }
    let plan = AllocationPlan::new(Probe).ok().expect("preflight");
    assert_eq!(plan.bytes(), 1024);
    assert!(!PREPARED.load(Ordering::SeqCst));
    let prepared = plan.prepare();
    assert_eq!(prepared.bytes(), 1024);
    assert!(PREPARED.load(Ordering::SeqCst));
}
