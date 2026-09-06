//! Compare the previous allocating estimator with direct serialized-byte sizing.

use std::{hint::black_box, time::Instant};

use rw_context::{LocalTokenEstimator, canonicalize_json};
use serde_json::{Value, json};

fn canonical_estimate(value: &Value) -> Result<u64, Box<dyn std::error::Error>> {
    Ok(u64::try_from(serde_json::to_vec(&canonicalize_json(value))?.len())?.div_ceil(4))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for bytes in [1024, 1024 * 1024, 8 * 1024 * 1024] {
        let value = json!({"z": "x".repeat(bytes), "a": ["\n\"🙂", 12.5, null]});
        assert_eq!(
            canonical_estimate(&value)?,
            LocalTokenEstimator::value(&value)
        );
        let mut before = Vec::new();
        let mut after = Vec::new();
        for trial in 0..101 {
            for direct in [trial % 2 == 0, trial % 2 != 0] {
                let start = Instant::now();
                let tokens = if direct {
                    LocalTokenEstimator::value(black_box(&value))
                } else {
                    canonical_estimate(black_box(&value))?
                };
                black_box(tokens);
                let elapsed = start.elapsed().as_nanos();
                if trial > 0 {
                    if direct {
                        after.push(elapsed);
                    } else {
                        before.push(elapsed);
                    }
                }
            }
        }
        let mut before_sorted = before.clone();
        let mut after_sorted = after.clone();
        before_sorted.sort_unstable();
        after_sorted.sort_unstable();
        println!(
            "{}",
            json!({"input_text_bytes": bytes, "samples": before.len(),
                "canonical_p50_ns": before_sorted[49], "canonical_p99_ns": before_sorted[98],
                "counted_p50_ns": after_sorted[49], "counted_p99_ns": after_sorted[98],
                "canonical_samples_ns": before, "counted_samples_ns": after})
        );
    }
    Ok(())
}
