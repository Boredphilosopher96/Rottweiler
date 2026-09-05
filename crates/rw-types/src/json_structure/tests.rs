#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{JsonStructureLimits, preflight_json};
fn limits() -> JsonStructureLimits {
    JsonStructureLimits {
        max_encoded_bytes: 1024,
        max_nodes: 32,
        max_string_bytes: 32,
        max_depth: 4,
    }
}
#[test]
fn borrowed_visit_counts_decoded_keys_and_text_before_container_construction() {
    let shape = preflight_json(br#"{"key":[1,true,"\u00e9"]}"#, limits()).expect("shape");
    assert_eq!(shape.nodes, 6);
    assert_eq!(shape.string_bytes, 5);
    assert_eq!(shape.depth, 2);
    assert!(shape.decode_bytes::<serde_json::Value>().expect("charge") > 5);
}
#[test]
fn dense_empty_containers_and_nesting_are_admitted_by_structure() {
    assert!(
        preflight_json(
            format!("[{}]", vec!["[]"; 33].join(",")).as_bytes(),
            limits()
        )
        .is_err()
    );
    assert!(preflight_json(b"[[[[[0]]]]]", limits()).is_err());
    assert!(preflight_json(b"null true", limits()).is_err());
    assert!(
        preflight_json(
            br#"{"key":"abcdefghijklmnopqrstuvwxyz0123456789"}"#,
            limits()
        )
        .is_err()
    );
}
