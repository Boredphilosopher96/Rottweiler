//! Exact-artifact worker policy comparison; workload construction is outside timers.
use super::{HookDispatcher, WasmHookLimits, WasmProcessHook, WasmWorkerPool, manifest};
use rw_ext::HookInput;
use rw_types::hook_contract::HookToolInput;
use std::{collections::BTreeMap, io::Read as _, sync::Arc, time::Instant};

const WARM_CALLS: usize = 32;
const CONCURRENT_CALLS: usize = 16;
const CONCURRENT_BATCHES: usize = 4;

struct Workload {
    component: Vec<u8>,
    input: HookInput,
    expected: HookInput,
    input_bytes: usize,
    output_bytes: usize,
}
impl Workload {
    fn new() -> Self {
        let input = HookInput::PreTool(HookToolInput {
            id: "capacity-call".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path":"fixture.rs", "content":"line é\n\\🦀".repeat(512)}),
        });
        let arguments = serde_json::json!({"summary":"résultat ✓ ".repeat(128),"complete":true});
        let output = serde_json::to_string(&serde_json::json!({
            "decision":"transform", "change":{"hook":"pre_tool","name":"read","arguments":arguments}
        }))
        .expect("expected directive bytes");
        let encoded = serde_json::to_string(&input).expect("exact input bytes");
        Self {
            component: super::component_for_input(&output, Some(&encoded)),
            input,
            expected: HookInput::PreTool(HookToolInput {
                id: "capacity-call".into(),
                name: "read".into(),
                arguments,
            }),
            input_bytes: encoded.len(),
            output_bytes: output.len(),
        }
    }
}

pub(super) async fn measure() {
    let rounds: usize = std::env::var("ROTTWEILER_WASM_BENCH_ROUNDS")
        .expect("explicit repeated round count")
        .parse()
        .expect("round count");
    assert!(
        (2..=20).contains(&rounds),
        "rounds must be between 2 and 20"
    );
    let (helper, receipt) = release_helper();
    let workload = Arc::new(Workload::new());
    println!(
        "{}",
        serde_json::json!({
            "workload":"byte-checked-transform", "helper_sha256":receipt.sha256,"helper_bytes":receipt.bytes,
            "component_sha256":super::sha256_bytes(&workload.component),
            "component_bytes":workload.component.len(),"input_bytes":workload.input_bytes,
            "directive_bytes":workload.output_bytes,"rounds":rounds,
            "warm_calls":WARM_CALLS,"concurrent_calls":CONCURRENT_CALLS,"concurrent_batches":CONCURRENT_BATCHES,
            "limits":WasmHookLimits::default(),"cold_calls":2,
        })
    );
    for round in 0..rounds {
        let order = if round % 2 == 0 { [1, 2] } else { [2, 1] };
        for workers in order {
            measure_round(round, workers, &helper, &workload).await;
        }
    }
}

pub(super) fn release_helper() -> (rw_tools::ApprovedExecutable, rw_tools::ExecutableDigest) {
    let receipt_path = std::path::PathBuf::from(
        std::env::var_os("ROTTWEILER_WASM_BENCH_RECEIPT").expect("exact release helper receipt"),
    )
    .canonicalize()
    .expect("receipt path");
    let mut bytes = Vec::new();
    std::fs::File::open(&receipt_path)
        .expect("receipt")
        .take(4097)
        .read_to_end(&mut bytes)
        .expect("bounded receipt read");
    assert!(bytes.len() <= 4096);
    let receipt: rw_tools::ExecutableDigest =
        serde_json::from_slice(&bytes).expect("typed receipt");
    let helper = rw_tools::ApprovedExecutable::from_installed(
        &receipt_path
            .parent()
            .expect("bundle")
            .join("rottweiler-wasm-host"),
        &receipt,
    )
    .expect("approved release helper");
    (helper, receipt)
}

async fn checked_call(dispatcher: &HookDispatcher, workload: &Workload) -> u128 {
    let input = workload.input.clone();
    let started = Instant::now();
    let result = dispatcher
        .dispatch(input)
        .await
        .expect("settled invocation");
    let elapsed = started.elapsed().as_micros();
    assert!(result.completed(), "policy must complete");
    assert!(
        result.failures().is_empty(),
        "worker failed: {:?}",
        result.failures()
    );
    assert_eq!(
        result.input(),
        &workload.expected,
        "exact transformed output"
    );
    elapsed
}

async fn measure_round(
    round: usize,
    workers: usize,
    helper: &rw_tools::ApprovedExecutable,
    workload: &Arc<Workload>,
) {
    let pool = WasmWorkerPool::with_worker_limit(workers).expect("capacity");
    let hook = WasmProcessHook::new(
        Arc::clone(&pool),
        helper.clone(),
        manifest(),
        workload.component.clone(),
        WasmHookLimits::default(),
    )
    .expect("proxy");
    let mut dispatcher = HookDispatcher::new();
    hook.register_hooks(&mut dispatcher)
        .expect("inert registration");
    let dispatcher = Arc::new(dispatcher);
    assert_eq!(pool.stats().process_starts, 0);
    let cold = Instant::now();
    let (first, second) = tokio::join!(
        checked_call(&dispatcher, workload),
        checked_call(&dispatcher, workload)
    );
    let cold_us = cold.elapsed().as_micros();
    let mut warm_us = Vec::new();
    for _ in 0..WARM_CALLS {
        warm_us.push(checked_call(&dispatcher, workload).await);
    }
    let mut batch_us = Vec::new();
    let mut concurrent_call_us = Vec::new();
    for _ in 0..CONCURRENT_BATCHES {
        let started = Instant::now();
        let mut jobs = Vec::new();
        for _ in 0..CONCURRENT_CALLS {
            let dispatcher = Arc::clone(&dispatcher);
            let workload = Arc::clone(workload);
            jobs.push(tokio::spawn(async move {
                checked_call(&dispatcher, &workload).await
            }));
        }
        let mut calls = Vec::new();
        for job in jobs {
            calls.push(job.await.expect("owned caller"));
        }
        batch_us.push(started.elapsed().as_micros());
        concurrent_call_us.push(calls);
    }
    let stats = pool.stats();
    assert_eq!(stats.process_starts, workers as u64);
    assert_eq!(stats.component_loads, workers as u64);
    assert_eq!(
        stats.cache_hits,
        (2 + WARM_CALLS + CONCURRENT_CALLS * CONCURRENT_BATCHES - workers) as u64
    );
    let rss = worker_rss(&pool, workers);
    let retirement = Instant::now();
    pool.shutdown().await.expect("all physical workers settled");
    let retirement_us = retirement.elapsed().as_micros();
    assert!(pool.idle_process_ids().is_empty());
    println!(
        "{}",
        serde_json::json!({
            "round":round,"workers":workers,"cold_pair_us":cold_us,"cold_call_us":[first,second],
            "warm_call_us":warm_us,"concurrent_batch_us":batch_us,"concurrent_call_us":concurrent_call_us,
            "warm_worker_rss_kib":rss,"warm_workers_rss_kib":rss.values().sum::<u64>(),
            "process_starts":stats.process_starts,"component_loads":stats.component_loads,"cache_hits":stats.cache_hits,
            "retirement_us":retirement_us,
        })
    );
}

fn worker_rss(pool: &WasmWorkerPool, workers: usize) -> BTreeMap<u32, u64> {
    let snapshot = std::process::Command::new("ps")
        .args(["-axo", "pid=,rss="])
        .output()
        .expect("RSS snapshot");
    assert!(snapshot.status.success());
    let pids = pool.idle_process_ids();
    assert_eq!(pids.len(), workers);
    let values = String::from_utf8(snapshot.stdout)
        .expect("ps text")
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let rss = parts.next()?.parse::<u64>().ok()?;
            pids.contains(&pid).then_some((pid, rss))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        values.len(),
        workers,
        "every worker must appear in the RSS snapshot"
    );
    assert!(values.values().all(|bytes| *bytes > 0));
    values
}

#[tokio::test]
async fn measurement_guest_checks_exact_input_bytes_and_output() {
    check_byte_oracle(super::fixture_helper()).await;
}

pub(super) async fn check_byte_oracle(helper: rw_tools::ApprovedExecutable) {
    let workload = Workload::new();
    let pool = WasmWorkerPool::with_worker_limit(1).expect("pool");
    let hook = WasmProcessHook::new(
        Arc::clone(&pool),
        helper,
        manifest(),
        workload.component.clone(),
        WasmHookLimits::default(),
    )
    .expect("proxy");
    let mut dispatcher = HookDispatcher::new();
    hook.register_hooks(&mut dispatcher).expect("registered");
    checked_call(&dispatcher, &workload).await;
    let mut changed = workload.input.clone();
    let HookInput::PreTool(input) = &mut changed else {
        panic!("tool input")
    };
    input.id = "capacity-calm".into();
    assert_eq!(
        serde_json::to_vec(&changed).expect("changed bytes").len(),
        workload.input_bytes
    );
    let result = dispatcher.dispatch(changed).await.expect("trap settled");
    assert_eq!(
        result.failures().len(),
        1,
        "same-length changed input must trap"
    );
    pool.shutdown().await.expect("physical retirement");
}
