#![allow(clippy::expect_used)]
use super::*;
use serde::Serializer;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Probe<'a> {
    budget: &'a ChildProgressBudget,
    calls: &'a AtomicUsize,
    fail: bool,
}
impl Serialize for Probe<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert!(
            self.budget.available_bytes()
                <= CHILD_PROGRESS_MEMORY_BYTES - 3 * MAX_CHILD_PROGRESS_BYTES
        );
        if self.fail {
            return Err(serde::ser::Error::custom("source failure"));
        }
        serializer.serialize_str("one\n\"é")
    }
}
#[test]
fn admission_precedes_single_serialization_and_saturation_never_invokes_source() {
    let budget = ChildProgressBudget::default();
    let calls = AtomicUsize::new(0);
    let source = Probe {
        budget: &budget,
        calls: &calls,
        fail: false,
    };
    let occupied = budget
        .reserve(CHILD_PROGRESS_MEMORY_BYTES)
        .expect("pressure");
    let marker = budget
        .construct(Some(1), &source)
        .expect("marker")
        .expect("canonical source");
    assert!(marker.value().is_null());
    assert!(
        budget
            .construct(None, &source)
            .expect("drop transient")
            .is_none()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    drop(occupied);
    let preview = budget
        .construct(Some(2), &source)
        .expect("encode")
        .expect("preview");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(preview.value(), &serde_json::json!("one\n\"é"));
    assert!(budget.available_bytes() < CHILD_PROGRESS_MEMORY_BYTES);
    drop(preview);
    assert_eq!(budget.available_bytes(), CHILD_PROGRESS_MEMORY_BYTES);
}
#[test]
fn encoding_errors_release_credit_and_preserve_source_error() {
    let budget = ChildProgressBudget::default();
    let calls = AtomicUsize::new(0);
    let error = budget
        .construct(
            Some(1),
            &Probe {
                budget: &budget,
                calls: &calls,
                fail: true,
            },
        )
        .expect_err("error");
    assert!(error.to_string().contains("source failure"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(budget.available_bytes(), CHILD_PROGRESS_MEMORY_BYTES);
}
#[test]
fn escaped_overflow_and_dense_decode_pressure_preserve_canonical_fences() {
    let budget = ChildProgressBudget::default();
    let text = "\n".repeat(MAX_CHILD_PROGRESS_BYTES / 2);
    assert!(
        budget
            .construct(Some(3), &text)
            .expect("invalidation")
            .expect("source")
            .value()
            .is_null()
    );
    assert!(budget.construct(None, &text).is_err());
    assert!(budget.construct(None, &Value::Null).is_err());
    // Encoded size fits, but the real array-capacity decode bound does not fit
    // alongside existing retained previews. Decode must never spend their credit.
    let occupied = budget
        .reserve(CHILD_PROGRESS_MEMORY_BYTES - 1024 * 1024)
        .expect("pressure");
    let dense = vec![0_u8; 60_000];
    assert!(
        budget
            .construct(Some(4), &dense)
            .expect("invalidation")
            .expect("source")
            .value()
            .is_null()
    );
    assert_eq!(budget.available_bytes(), 1024 * 1024);
    drop(occupied);
    assert_eq!(budget.available_bytes(), CHILD_PROGRESS_MEMORY_BYTES);
}
#[test]
fn exact_json_containers_and_depth_rejection_are_bounded() {
    let budget = ChildProgressBudget::default();
    let value = serde_json::json!({"map": {"é": "\0\n\""}, "array": [null, true, 12.5, []]});
    let preview = budget
        .construct(Some(1), &value)
        .expect("encode")
        .expect("preview");
    assert_eq!(preview.value(), &value);
    let mut deep = Value::Null;
    for _ in 0..63 {
        deep = Value::Array(vec![deep]);
    }
    assert!(
        budget
            .construct(Some(2), &deep)
            .expect("invalidation")
            .expect("source")
            .value()
            .is_null()
    );
    assert!(budget.construct(None, &deep).is_err());
    let other = ChildProgressBudget::default();
    assert!(budget.owns(&preview));
    assert!(!other.owns(&preview));
}
#[tokio::test]
async fn cancelled_observer_retains_body_until_its_actual_future_is_destroyed() {
    let budget = ChildProgressBudget::default();
    let preview = budget
        .construct(Some(1), &"retained".repeat(1024))
        .expect("encode")
        .expect("preview");
    let used = budget.available_bytes();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = entered.send(());
        std::future::pending::<()>().await;
        drop(preview);
    });
    ready.await.expect("started");
    assert_eq!(budget.available_bytes(), used);
    task.abort();
    let _ = task.await;
    assert_eq!(budget.available_bytes(), CHILD_PROGRESS_MEMORY_BYTES);
}
#[test]
fn delivery_keeps_credit_through_consumer_admission_and_panics() {
    let budget = ChildProgressBudget::default();
    for fail in [false, true] {
        let preview = budget
            .construct(Some(1), &"payload")
            .expect("encode")
            .expect("preview");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            preview.deliver(|value| {
                assert!(budget.available_bytes() < CHILD_PROGRESS_MEMORY_BYTES);
                assert_eq!(value, serde_json::json!("payload"));
                assert!(!fail, "consumer failure");
                drop(value);
                assert!(budget.available_bytes() < CHILD_PROGRESS_MEMORY_BYTES);
            });
        }));
        assert_eq!(result.is_err(), fail);
        assert_eq!(budget.available_bytes(), CHILD_PROGRESS_MEMORY_BYTES);
    }
}

#[test]
fn finite_numbers_keep_exact_bits_and_json_through_the_public_value_path() {
    let budget = ChildProgressBudget::default();
    for number in [
        0.0_f64,
        -0.0,
        0.845_512_408_225_570_1,
        1.234_567_890_123_456_7,
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::from_bits(1),
        f64::from_bits(0x3fd5_5555_5555_5555),
    ] {
        let original = serde_json::json!({"number": number});
        let preview = budget
            .encode_value(Some(1), &original)
            .expect("encode")
            .expect("preview");
        assert_eq!(
            preview.value()["number"].as_f64().expect("float").to_bits(),
            number.to_bits()
        );
        assert_eq!(
            serde_json::to_vec(preview.value()).expect("wire"),
            serde_json::to_vec(&original).expect("source wire")
        );
    }
    let integers = serde_json::json!([i64::MIN, u64::MAX]);
    let preview = budget
        .encode_value(Some(1), &integers)
        .expect("encode")
        .expect("preview");
    assert_eq!(preview.value(), &integers);
}
