# Typed tool lifetime and progress: A23

Plugin tools now have an explicit operation lifetime. The host admits a typed
`tool/call` with immutable total and idle durations; progress renews only idle
time. The current host policy grants at most five minutes total and defaults to
90 seconds idle. Hook, catalog, and control RPCs retain their five-second limit.
Generic RPC dispatch rejects `tool/call`, so a caller cannot accidentally select
the ordinary request policy. Protocol 3 and all consumers migrate together;
there is no older-wire adapter.

## Ownership and bounds

`rw-operation-contract` owns the validated lifetime, progress values, and shared
limits. Both Rust protocols depend on this leaf crate. Generated SDK types and
JSON schemas project that owner. Relational checks (`completed <= total` and
`idle <= total`) run at typed boundaries because standard JSON Schema does not
express those sibling-field comparisons.

The host starts monotonic deadlines after admission, before writer queueing.
The existing five-second admission/write budgets remain separate. A response
received after total or idle expiry cannot turn the operation into a success,
even if its observer has not polled yet. SDK timers mirror the admitted policy;
the Rust host remains authoritative. Cancellation, timeout, and future drop use
the existing owned process/effect settlement barrier. A shared native plugin
process is still the failure boundary: cooperative JavaScript cancellation does
not establish that ambient native effects stopped.

Each SDK operation retains one current write and one replaceable pending update.
The progress lane has 64 frame slots and 64 × 4097 bytes of admission, including
the current physical write. Control traffic has priority over progress, which
has priority over provider data between writes. A blocked physical write remains
subject to its existing timeout. Progress messages contain at most 256 Unicode
characters/1024 UTF-8 bytes, without control characters, and optional bounded
32-bit work counts. The SDK emits at most four updates per second; the host
validates increasing sequence numbers and a four-token rate bucket. Invalid or
unadmitted progress fails the connection rather than granting execution time.

`ToolContext` exposes a synchronous progress sink. Engine admission retains one
replaceable update and at most one queued signal per invocation, with a 250 ms
enqueue interval. Closing or dropping the invocation owner revokes retained
sinks and discards pending progress. Updates are best-effort observations;
coalescing does not guarantee delivery of every update or the last update.
`ToolProgress` is a transient client event and never enters the journal. The TUI
validates and ignores it today; rendering progress is separate product work.
This is a per-operation bound within existing tool/RPC admission, not a new
global multi-session resource governor.

## Invocation identity and recovery

Every tool start, output, diff, approval, and finish carries a host-owned
`ToolInvocationId`, distinct from the provider call ID. IDs include the host turn
and invocation coordinates and are unique within a session. Approval replies
must match that identity; a stale reply cannot consume a newer pending approval.
The TUI rejects stale lifecycle updates when a provider reuses its call ID.

Failed attempts emit their start before their finish, including missing tool
arguments. Interrupted recovery finishes an already-recorded invocation. If IR
contains a pending call without a start, recovery emits a paired start/finish.
Synthesized IDs use the original IR call ordinal, so a crash after repairing the
first of several calls does not renumber subsequent repairs. A recorded start
without committed IR receives a historical finish without inventing a model
tool result. These changes consume the A02 paged `SessionProjector` contract.

## Reproducible verification

Run in a checkout with its own Cargo target and the pinned Bun 1.3.14 on PATH:

```sh
cargo test -p rw-core -p rw-ext -p rw-runtime -p rw-operation-contract -p rw-plugin-protocol --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run --locked --quiet -p xtask -- codegen --check
cargo run --locked --quiet -p rw-plugin-protocol --bin rw-plugin-protocol-codegen -- --check
cargo fmt --all --check
python3 scripts/check-ownership.py
python3 scripts/check-dependency-direction.py
cargo check --manifest-path fuzz/Cargo.toml --bin plugin_rpc --bin event_log --locked
```

In `packages/plugin-sdk`, run `bun run typecheck`, `bun test`, and `bun run build`.
Then refresh the file dependency in `packages/plugin-host` with
`bun install --frozen-lockfile --force` and run its typecheck, tests, and build.
In `packages/tui`, run `bun run typecheck` and `bun run test`.

Local macOS validation passed 316 core unit tests, 132 extension unit tests,
218 runtime unit tests, the shared contract test, and four plugin protocol
contract tests, plus the selected crates' integration tests. Four existing
soak/long tests remain ignored. Pinned Bun passed 70 SDK tests, four plugin-host
tests, 532 TUI tests with 21 snapshots, all three typechecks, and both plugin
package builds. Documentation checks passed 23 page/projection pairs and five
projection tests. Workspace all-feature/all-target clippy, generation, ownership,
dependency-direction, and formatting checks passed.

Regressions exercise real Rust RPC framing with progress beyond the ordinary
deadline, idle renewal with an immutable total deadline, silent native parent
and child mutation followed by idle timeout and verified settlement, 100,000
coalesced updates, delayed physical writes, concurrent SDK control replies,
stale approval/output/final identities, reused provider IDs, and a crash during
recovery. Deadline tests use short durations to exercise the same policy; a
60-second qualification run, native Linux run, performance calibration, hosted
CI, and release/soak qualification are not claimed here.

Unicode validation adds an AJV runtime helper. The generator now bundles that
helper into the standalone protocol validator, avoiding dependence on a
consumer's `node_modules` layout. Unicode and trailing-newline regressions pass.
The generated source is 545,167 bytes versus 637,710 previously; this is a source
artifact measurement, not a startup or render-performance claim.
