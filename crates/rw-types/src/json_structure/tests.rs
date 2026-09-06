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

#[test]
fn direct_value_admission_charges_actual_container_shapes() {
    use crate::allocation::PrepareAllocation as _;
    for value in [
        serde_json::json!({"a":[1,2,3], "b":{"c":"text"}}),
        serde_json::json!(
            (0..1024)
                .map(|n| serde_json::json!({"x":n, "y":[true, null, "v"]}))
                .collect::<Vec<_>>()
        ),
        serde_json::json!({"wide": (0..1024).map(|n| (n.to_string(), serde_json::json!(n))).collect::<std::collections::BTreeMap<_,_>>()}),
    ] {
        let bytes = serde_json::to_vec(&value).expect("JSON");
        let shape = super::preflight_json(
            &bytes,
            super::JsonStructureLimits {
                max_encoded_bytes: 1024 * 1024,
                max_nodes: 32768,
                max_string_bytes: 1024 * 1024,
                max_depth: 32,
            },
        )
        .expect("preflight");
        let mut decoded: serde_json::Value = serde_json::from_slice(&bytes).expect("decode");
        decoded.prepare_allocations();
        assert!(
            shape.direct_value_decode_bytes().expect("decode bound")
                >= decoded.prepared_bytes().expect("retained bound")
        );
    }
    let shape = super::preflight_json(
        br#"{"a":[1,2,3],"b":{"c":"text"}}"#,
        super::JsonStructureLimits {
            max_encoded_bytes: 1024,
            max_nodes: 128,
            max_string_bytes: 1024,
            max_depth: 8,
        },
    )
    .expect("shape");
    assert_eq!(
        (
            shape.objects,
            shape.object_entries,
            shape.arrays,
            shape.array_entries
        ),
        (2, 3, 1, 3)
    );
}
