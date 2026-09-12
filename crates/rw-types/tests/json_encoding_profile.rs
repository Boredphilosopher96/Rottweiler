//! Matched serializer destination measurements; run the compiled release test
//! with --ignored --nocapture. Corpus construction and byte verification are
//! outside timed regions. Every round reports its own observation.
#![allow(clippy::expect_used)]
#[path = "json_encoding_profile/support.rs"]
mod support;
use rw_types::{EngineEvent, json_encoding::JsonWriter};
use std::{hint::black_box, time::Instant};
use support::{ReferenceBuffer, ReferenceCount, corpus};

#[derive(Clone, Copy, Debug)]
enum Case {
    ReferenceCount,
    SharedCount,
    ReferenceBuffer,
    SharedBuffer,
    ReferenceStream,
    SharedStream,
}
const CASES: [Case; 6] = [
    Case::ReferenceCount,
    Case::SharedCount,
    Case::ReferenceBuffer,
    Case::SharedBuffer,
    Case::ReferenceStream,
    Case::SharedStream,
];
const LIMIT: usize = 1024 * 1024;

fn encode(case: Case, event: &EngineEvent) -> (usize, Vec<u8>) {
    match case {
        Case::ReferenceCount => {
            let mut count = ReferenceCount::default();
            serde_json::to_writer(&mut count, event).expect("reference count");
            (count.0, Vec::new())
        }
        Case::SharedCount => {
            let mut count = JsonWriter::count(LIMIT);
            count.serialize(event).expect("shared count");
            (count.written(), Vec::new())
        }
        Case::ReferenceBuffer => {
            let mut output = ReferenceBuffer::default();
            serde_json::to_writer(&mut output, event).expect("reference buffer");
            (output.0.len(), output.0)
        }
        Case::SharedBuffer => {
            let mut bytes = Vec::new();
            JsonWriter::buffer(&mut bytes, LIMIT, 1024)
                .expect("buffer admission")
                .serialize(event)
                .expect("shared buffer");
            (bytes.len(), bytes)
        }
        Case::ReferenceStream => {
            let mut bytes = Vec::new();
            serde_json::to_writer(&mut bytes, event).expect("reference stream");
            (bytes.len(), bytes)
        }
        Case::SharedStream => {
            let mut bytes = Vec::new();
            JsonWriter::stream(&mut bytes, usize::MAX)
                .serialize(event)
                .expect("shared stream");
            (bytes.len(), bytes)
        }
    }
}

fn verify(events: &[EngineEvent]) -> usize {
    let mut total = 0;
    for event in events {
        let expected = serde_json::to_vec(event).expect("reference JSON bytes");
        total += expected.len();
        for case in CASES {
            let (length, bytes) = encode(case, event);
            assert_eq!(length, expected.len(), "{case:?}");
            if !matches!(case, Case::ReferenceCount | Case::SharedCount) {
                assert_eq!(bytes, expected, "{case:?}");
            }
        }
    }
    total
}

#[test]
fn typed_event_destinations_have_identical_byte_oracles() {
    verify(&corpus());
}

fn repetitions(variable: &str, default: &str) -> usize {
    let count = std::env::var(variable)
        .unwrap_or_else(|_| default.into())
        .parse()
        .expect("positive repetition count");
    assert!((1..=1_000_000).contains(&count));
    count
}

fn measure(case: Case, events: &[EngineEvent], repetitions: usize) -> (u128, usize) {
    let started = Instant::now();
    let mut total = 0;
    for _ in 0..repetitions {
        for event in events {
            let (length, bytes) = encode(case, black_box(event));
            total += black_box(length);
            black_box(bytes);
        }
    }
    (started.elapsed().as_nanos(), total)
}

#[test]
#[ignore = "matched release CPU measurement; prints per-round observations"]
fn matched_json_destination_cpu() {
    let events = corpus();
    let small = [events[0].clone(), events[1].clone(), events[4].clone()];
    let body = [events[2].clone(), events[3].clone(), events[5].clone()];
    let workloads = [
        (
            "small",
            &small,
            repetitions("RW_JSON_SMALL_REPETITIONS", "10000"),
        ),
        ("body", &body, repetitions("RW_JSON_REPETITIONS", "1000")),
    ];
    let oracles = [verify(&small), verify(&body)];
    for round in 0..8 {
        for index in 0..workloads.len() {
            let workload = (index + round) % workloads.len();
            let (name, events, repetitions) = workloads[workload];
            for offset in 0..CASES.len() {
                let case = CASES[(offset + round) % CASES.len()];
                let (elapsed, total) = measure(case, events, repetitions);
                assert_eq!(total, oracles[workload] * repetitions);
                println!(
                    "{{\"round\":{round},\"warmup\":{},\"workload\":\"{name}\",\"case\":\"{case:?}\",\"calls\":{},\"encoded_bytes\":{total},\"elapsed_ns\":{elapsed}}}",
                    round < 2,
                    repetitions * events.len(),
                );
            }
        }
    }
    verify(&events);
}
