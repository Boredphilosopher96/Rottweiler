//! One record and one returned page share a bounded typed decode allowance.
use super::{MAX_JOURNAL_DECODE_BYTES, MAX_SEGMENT_BYTES, SessionStoreError};
use rw_types::{
    EngineEvent,
    allocation::DecodeAllocation,
    json_structure::{JsonStructureLimits, preflight_json},
};

pub(in crate::session) fn preflight_record<T: DecodeAllocation>(
    line: &[u8],
) -> Result<usize, SessionStoreError> {
    let shape = preflight_json(
        line,
        JsonStructureLimits {
            max_encoded_bytes: MAX_SEGMENT_BYTES,
            max_nodes: 65_536,
            max_string_bytes: MAX_SEGMENT_BYTES,
            max_depth: 64,
        },
    )?;
    let typed = shape.decode_bytes::<T>();
    let canonical = shape.decode_bytes::<EngineEvent>();
    let charge = typed
        .zip(canonical)
        .map(|(typed, canonical)| typed.max(canonical))
        .ok_or(SessionStoreError::CorruptEvent(
            "journal decode charge overflow",
        ))?;
    if charge > MAX_JOURNAL_DECODE_BYTES {
        return Err(SessionStoreError::CorruptEvent(
            "journal record exceeds decoded allocation admission",
        ));
    }
    Ok(charge)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::preflight_record;
    use crate::session::{SessionEventPageLimits, journal::SegmentedJournal};
    use rw_types::{
        allocation::AllocationPlan,
        json_structure::{JsonStructureLimits, preflight_json},
    };
    use serde_json::{Value, json};

    #[test]
    fn producer_rejects_dense_record_before_advancing_the_committed_prefix() {
        let root = tempfile::tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "dense").expect("journal");
        let event = json!({"items":vec![Value::Null;65_536]});
        assert!(journal.append_batch([event]).is_err());
        assert_eq!(journal.read_view().prefix_identity().next_sequence, 0);
        journal
            .append_batch([json!({"accepted":true})])
            .expect("subsequent valid append");
        assert_eq!(journal.read_view().prefix_identity().next_sequence, 1);
    }
    #[test]
    fn page_admission_counts_decoded_containers_separately_from_encoded_bytes() {
        let root = tempfile::tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "pages").expect("journal");
        let event = json!({"items":vec![Value::Null;2048]});
        for _ in 0..4 {
            journal
                .append_batch([event.clone()])
                .expect("bounded record");
        }
        let view = journal.read_view();
        let page = view
            .page::<Value>(None, SessionEventPageLimits::default())
            .expect("page");
        assert!(!page.events.is_empty());
        assert!(
            page.events.len() < 4,
            "small encoding must not bypass decoded page charge"
        );
        assert!(page.has_more);
        let next = view
            .page::<Value>(page.next_cursor, SessionEventPageLimits::default())
            .expect("next page");
        assert!(!next.events.is_empty());
    }
    #[test]
    fn structural_charge_covers_normalized_json_allocation_with_escaped_text_and_maps() {
        let value = json!({"nested":[{"a":"é\\\"\n"},{"b":[true,false,null]}]});
        let bytes = serde_json::to_vec(&value).expect("encoding");
        let shape = preflight_json(
            &bytes,
            JsonStructureLimits {
                max_encoded_bytes: 1024,
                max_nodes: 128,
                max_string_bytes: 1024,
                max_depth: 16,
            },
        )
        .expect("shape");
        let charge = shape.decode_bytes::<Value>().expect("decode charge");
        let retained = AllocationPlan::new(value).expect("prepared plan").bytes();
        assert!(charge >= retained);
        assert!(preflight_record::<Value>(&bytes).expect("canonical admission") >= charge);
    }
}
