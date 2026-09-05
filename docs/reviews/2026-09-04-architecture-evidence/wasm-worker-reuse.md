# WASM worker reuse: A22

ADR-032 replaces per-call process startup and component compilation with an
application-owned pool. `RuntimeSessionFactory` shares one pool across its
hosted sessions; the standalone CLI owns its pool. Construction starts no helper.
Factory shutdown closes admission, cancels work, and awaits job and process cleanup.
The previous public one-shot helper API is deleted.

Each pool admits 32 requests and has a provisional two-worker ceiling. Tokio's
execution semaphore queues admitted requests in FIFO order. The fixed five-second
operation deadline includes queue wait, component loading, writes, and response
reads. Excess admission fails immediately. Each admitted call owns one bounded
input string and shares its component bytes through `Arc`; only active workers
construct wire buffers. Existing activation bounds remain 32 enabled components
and 64 MiB aggregate component bytes.

Each worker caches one immutable compiled generation. The key includes the exact
component digest, manifest, and invocation limits; the helper path selects the
private executable. The running helper fixes its target, engine version, and
configuration. Compiled artifacts never move between processes or enter a disk
cache, so untrusted serialized machine code is never deserialized. Free slots
retain another generation before evicting the oldest idle entry. Working sets
larger than the worker count incur cache misses; increasing retention requires
separate memory measurements.

The helper handles sequential load/call frames. Warm calls transfer only event
and bounded input. Each invocation still creates a fresh Wasmtime store and
instance, preserving no-import/WASI restrictions, memory/table/instance limits,
fuel, and output bounds. A test component traps if its instance is invoked twice;
repeated calls through the persistent helper succeed because instances are fresh.

Caller cancellation or future drop signals an owned job. The job retains its
worker/admission permits through kill and actual reap. Hook settlement waits only
for abandoned jobs of that hook generation, not unrelated live calls. Completed
entries remove themselves. Traps, timeout, invalid components, malformed output,
and failed IO retire the worker before replacement. A reap error leaves settlement
unproven rather than releasing the slot. This process-only proof applies to the
trusted private helper whose guest interface cannot spawn native descendants;
ambient native RPC plugins retain their separate process-tree settlement owner.

Explicit shutdown is the awaitable cleanup boundary. If the last pool owner is
dropped with a Tokio runtime available, an owned task reaps idle workers. Dropping
a pool after its executor is gone can only request child termination; it cannot
claim an awaited reap. Embedders must shut down their application host before
ending its executor.

## Checks and remaining qualification

```sh
cargo test -p rw-ext -p rw-wasm-host --all-targets --all-features
cargo test -p rw-runtime wasm --lib --all-features
cargo clippy -p rw-ext -p rw-wasm-host -p rw-runtime -p rw-cli --all-targets --all-features -- -D warnings
cargo fmt --all --check
python3 scripts/check-ownership.py
```

Local functional checks passed 133 extension tests, four actual helper-process
integration tests, and two runtime composition tests. The regressions cover cache reuse, fresh instances, exact
manifest/fuel identity, bounded eviction, trap recovery, all 32 admission slots,
dropped callers, immutable timeout, process reap, and self-retiring cleanup
records. All-target/all-feature scoped clippy and ownership checks passed.

A reproducible ignored measurement prints raw cold, warm, 16-way concurrent, and
post-workload helper RSS samples for one and two workers:

```sh
cargo build --release -p rw-wasm-host
ROTTWEILER_WASM_BENCH_HELPER="$PWD/target/release/rottweiler-wasm-host" cargo test -p rw-wasm-host --test protocol worker_capacity_measurement -- --ignored --nocapture
```

Run this alone on an idle native machine. The two-worker default is provisional;
no timing, RSS ceiling, Linux, hosted CI, or release qualification is claimed by
this checkpoint. Startup still validates enabled WASM components in the existing
runtime composition path; A28 separately removes that eager compilation work.
