//! Identical context workloads with separate timing and allocation executables.
use super::*;

#[cfg(feature = "allocation-measurement")]
#[global_allocator]
static ALLOCATOR: &stats_alloc::StatsAlloc<std::alloc::System> = &stats_alloc::INSTRUMENTED_SYSTEM;

const TURNS: usize = 128;
const ROWS: usize = 16;
const SAMPLES: usize = 500;
const WARMUPS: usize = 5;

fn workload(value_bytes: usize) -> Vec<Turn> {
    (0..TURNS)
        .map(|index| Turn {
            role: Role::Tool,
            blocks: vec![Block::ToolResult {
                id: ToolCallId(format!("call-{index}")),
                is_error: false,
                output: ToolOutput::Structured {
                    value: serde_json::json!({"rows": (0..ROWS).map(|row| {
                        serde_json::json!({"row":row,"value":format!("item {index}: αβ\\\"\n{}", "x".repeat(value_bytes))})
                    }).collect::<Vec<_>>()}),
                },
            }],
            meta: TurnMeta::default(),
        })
        .collect()
}

struct Sample {
    #[cfg(not(feature = "allocation-measurement"))]
    started: std::time::Instant,
    #[cfg(feature = "allocation-measurement")]
    allocation: stats_alloc::Region<'static, std::alloc::System>,
}
impl Sample {
    fn begin() -> Self {
        Self {
            #[cfg(not(feature = "allocation-measurement"))]
            started: std::time::Instant::now(),
            #[cfg(feature = "allocation-measurement")]
            allocation: stats_alloc::Region::new(ALLOCATOR),
        }
    }
    fn finish(self) -> serde_json::Value {
        #[cfg(not(feature = "allocation-measurement"))]
        {
            let elapsed_ns = self.started.elapsed().as_nanos();
            serde_json::json!({"elapsed_ns":elapsed_ns})
        }
        #[cfg(feature = "allocation-measurement")]
        {
            let counters = self.allocation.change();
            serde_json::json!({
                "allocations":counters.allocations,
                "deallocations":counters.deallocations,
                "reallocations":counters.reallocations,
                "bytes_allocated":counters.bytes_allocated,
                "bytes_deallocated":counters.bytes_deallocated,
                "bytes_reallocated":counters.bytes_reallocated
            })
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn run() {
    let mode = std::env::var("ROTTWEILER_CONTEXT_MEASURE_MODE").unwrap_or_else(|_| "paired".into());
    assert!(matches!(mode.as_str(), "paired" | "cached" | "full"));
    let value_bytes = std::env::var("ROTTWEILER_CONTEXT_MEASURE_VALUE_BYTES")
        .map_or(128, |value| {
            value.parse::<usize>().expect("row value bytes")
        });
    assert!(matches!(value_bytes, 128 | 512 | 2048));
    let root = tempfile::tempdir().expect("root");
    let config = fixture(root.path(), 2);
    let conversation = workload(value_bytes);
    let sources = (1..=TURNS).map(|id| source(id as u64)).collect::<Vec<_>>();
    let queued = VecDeque::new();
    let pruned = BTreeMap::new();
    let mut working = (mode != "full").then(|| {
        admit(
            super::super::fixtures::history::working_allowance(()),
            &config,
            &conversation,
            &sources,
            &queued,
        )
        .expect("working")
    });
    // Oracle construction and serialization are outside every sample. A full-only
    // process never creates a retained cache; cached/full RSS can be compared.
    let oracle = if let Some(working) = &working {
        assemble_session_context(
            &config,
            working,
            &conversation,
            &sources,
            &queued,
            &[],
            &pruned,
        )
    } else {
        assemble_full_session_context(&config, &conversation, &sources, &queued, &[], &pruned)
    }
    .expect("oracle");
    let cached_bytes = request_bytes(&config, oracle);
    if mode == "paired" {
        let full =
            assemble_full_session_context(&config, &conversation, &sources, &queued, &[], &pruned)
                .expect("full oracle");
        assert_eq!(
            cached_bytes,
            request_bytes(&config, full),
            "identical provider request"
        );
    }
    let request_hash = blake3::hash(&cached_bytes).to_hex().to_string();
    let request_bytes = cached_bytes.len();
    let source_bytes = serde_json::to_vec(&conversation)
        .expect("source bytes")
        .len();
    drop(cached_bytes);
    eprintln!(
        "context_measurement,{}",
        serde_json::json!({
            "schema_version":1,"mode":mode,"allocation_instrumented":cfg!(feature="allocation-measurement"),
            "profile":if cfg!(debug_assertions) {"debug"} else {"release"},
            "turns":TURNS,"rows_per_turn":ROWS,"row_value_bytes":value_bytes,
            "source_serialized_bytes":source_bytes,"request_bytes":request_bytes,
            "request_blake3":request_hash,"samples_per_implementation":SAMPLES,"warmups_per_implementation":WARMUPS
        })
    );
    for index in 0..SAMPLES + WARMUPS {
        for cached in if index % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        } {
            if (mode == "cached" && !cached) || (mode == "full" && cached) {
                continue;
            }
            let sample = Sample::begin();
            let full_working = if cached {
                working = Some(
                    readmit(
                        working.take().expect("cache owner"),
                        &config,
                        &conversation,
                        &sources,
                        &queued,
                    )
                    .expect("cached profile"),
                );
                None
            } else {
                Some(
                    admit(
                        super::super::fixtures::history::working_allowance(()),
                        &config,
                        &conversation,
                        &sources,
                        &queued,
                    )
                    .expect("full profile"),
                )
            };
            let result = if cached {
                assemble_session_context(
                    &config,
                    working.as_ref().expect("cache owner"),
                    &conversation,
                    &sources,
                    &queued,
                    &[],
                    &pruned,
                )
            } else {
                assemble_full_session_context(
                    &config,
                    &conversation,
                    &sources,
                    &queued,
                    &[],
                    &pruned,
                )
            }
            .expect("assembly");
            std::hint::black_box(&result);
            std::hint::black_box(&full_working);
            let counters = sample.finish();
            let Some(index) = index.checked_sub(WARMUPS) else {
                continue;
            };
            eprintln!(
                "context_sample,{}",
                serde_json::json!({
                    "sample":index,"cached":cached,"counters":counters
                })
            );
        }
    }
}
