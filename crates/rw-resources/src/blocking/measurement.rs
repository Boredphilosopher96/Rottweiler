//! Source-matched worker scheduling comparison, compiled explicitly before running.
#![allow(clippy::expect_used)]
use super::*;
use std::{hint::black_box, time::Instant};

const SAMPLES: usize = 200;
const ROUNDS: usize = 6;
const WARMUP_ROUNDS: usize = 2;

#[derive(Clone, Copy, Debug)]
enum Backend {
    Typed,
    Shared,
}
#[derive(Debug, PartialEq)]
struct Output {
    checksum: u64,
    bytes: Vec<u8>,
}

// One identical application kernel is used by both scheduling backends.
#[inline(never)]
fn transform(seed: u64, input: &[u8]) -> Output {
    let mut checksum = seed;
    for _ in 0..128 {
        checksum = checksum.wrapping_mul(6364136223846793005).wrapping_add(1);
    }
    let key = seed.to_le_bytes()[0];
    let bytes = input.iter().map(|value| value ^ key).collect();
    Output { checksum, bytes }
}

async fn typed_baseline<T: Send + 'static>(
    pool: &Pool,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, WorkError> {
    let lease = admit(pool, ResourceClass::Cpu).await?;
    let span = tracing::Span::current();
    Ok(tokio::task::spawn_blocking(move || {
        let _lease = lease;
        span.in_scope(work)
    })
    .await?)
}

async fn sample(pool: &Pool, backend: Backend, seed: u64, input: Arc<[u8]>) -> (Output, u128) {
    let started = Instant::now();
    let output = match backend {
        Backend::Typed => typed_baseline(pool, move || transform(black_box(seed), &input)).await,
        Backend::Shared => {
            run(pool, ResourceClass::Cpu, move || {
                transform(black_box(seed), &input)
            })
            .await
        }
    }
    .expect("worker completed");
    (output, started.elapsed().as_nanos())
}

#[tokio::test]
#[ignore = "prebuilt release worker scheduling comparison; paired small and 64KiB outputs"]
async fn qualify_shared_blocking_worker() {
    assert!(
        !black_box(cfg!(debug_assertions)),
        "prebuild the release harness"
    );
    let pool = Pool::new(1, 64);
    let small: Arc<[u8]> = Arc::from([]);
    let body: Arc<[u8]> = (0..64 * 1024_usize)
        .map(|index| index.to_le_bytes()[0])
        .collect::<Vec<_>>()
        .into();
    let workloads = [("small", small), ("body", body)];
    for round in 0..ROUNDS {
        for offset in 0..workloads.len() {
            let (name, input) = &workloads[(offset + round) % workloads.len()];
            for ordinal in 0..SAMPLES {
                let seed = u64::try_from(round * SAMPLES + ordinal).expect("sample identity");
                let expected_bytes = input
                    .iter()
                    .map(|value| value ^ seed.to_le_bytes()[0])
                    .collect::<Vec<_>>();
                let order = if (round + ordinal) % 2 == 0 {
                    [Backend::Typed, Backend::Shared]
                } else {
                    [Backend::Shared, Backend::Typed]
                };
                // Prepare caller input ownership outside both timed intervals.
                let first_input = input.clone();
                let second_input = input.clone();
                let (first, first_ns) = sample(&pool, order[0], seed, first_input).await;
                let (second, second_ns) = sample(&pool, order[1], seed, second_input).await;
                assert_eq!(first, second, "paired worker result changed");
                assert_eq!(first.bytes, expected_bytes, "full output byte oracle");
                assert_eq!(pool.execution.available_permits(), 1);
                assert_eq!(pool.waiting.available_permits(), 64);
                for (backend, elapsed) in [(order[0], first_ns), (order[1], second_ns)] {
                    println!(
                        "worker_measurement {{\"round\":{round},\"warmup\":{},\"workload\":\"{name}\",\"case\":\"{backend:?}\",\"sample\":{ordinal},\"elapsed_ns\":{elapsed},\"input_bytes\":{},\"output_bytes\":{},\"checksum\":{}}}",
                        round < WARMUP_ROUNDS,
                        input.len(),
                        first.bytes.len(),
                        first.checksum,
                    );
                }
            }
        }
    }
}
