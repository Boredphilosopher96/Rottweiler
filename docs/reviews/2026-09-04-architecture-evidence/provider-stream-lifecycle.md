# Provider stream lifecycle: protocol 3 checkpoint

This checkpoint completes the provider-stream part of A06 and extends A21's local
effect-settlement boundary through provider routing and engine completion. A24's
correlated host outcomes were implemented in the preceding checkpoints. A23's
long-running ordinary tool requests and typed progress remain unfinished: tool,
hook, and catalog handlers still have the existing five-second deadline.

The subsequent A23 checkpoint replaces the tool-only five-second behavior; see
[tool lifetime and progress evidence](tool-lifetime-progress.md). The observation
above describes this provider checkpoint at its original commit.

ADR-031 is the contract decision. Protocol 3 replaces protocol 2 directly. The
old generated SDK module, wire fixture, schema, and `provider/cancel` method are
removed. Host-mediated HTTP retains its distinct `provider/http_cancel` operation:
the host owns that request's socket/task authority. Native provider cancellation
uses process-wide teardown because an SDK acknowledgement cannot establish that
ambient native work stopped.

## Ownership and bounds

- A provider process admits four streams. Each has 64 events and 4 MiB of wire
  payload credit, plus reserved canonical terminal storage. A fixed five-minute
  deadline includes consumer pauses; returned credits cannot extend it.
- Credit bytes are the exact received UTF-8 JSON frame length excluding LF,
  including envelope bytes. The decoder retains that count before parsing.
  JavaScript and Rust can spell the same numeric value differently; reserializing
  the parsed value must never determine a refund.
- Encoded control buffers have a 64-frame, 16 MiB queue budget. The data lane has
  a separate 16 MiB plus four newline-byte budget. The Rust HTTP data queue has
  64 frame slots; SDK providers hold one pending data write each. The active
  physical write retains its byte permit. Rust calculates serialized size without
  an encoded allocation, then reserves bytes before allocating exact capacity.
  This requires a counting serialization pass. These are encoded-buffer bounds,
  not a claim that the whole process or parsed JSON occupies only 16 MiB.
- Control traffic has priority between physical writes. Data producers await the
  actual write before enqueueing their terminal reply. An individual blocked
  pipe write still has the write deadline; priority cannot preempt bytes already
  being written.
- The host holds `finished` until the correlated null RPC success is validated,
  and drains preceding data before exposing it. An engine that stops at `finished`
  cannot accidentally cancel a successfully completed native operation.
- The router retains the exact invoked provider, including across catalog changes.
  Its registry has 64 active/unsettled entries. Completed cleanup removes its own
  entry; abandoned owners remain charged. A dropped outer future starts owned
  cleanup. A destructor panic or lost cleanup owner leaves proof pending, with
  the provider still retained, rather than silently releasing the barrier.
- Engine main inference, compaction, title collection, and the outer turn-complete
  boundary await local settlement. Recording settlement waits behind already
  admitted writes without consuming the recorder's first-error report. Live
  unrelated provider streams are not treated as abandoned work to drain.

Settlement means host-owned local effects have stopped or completed. It does not
establish that remote inference stopped or that final remote billing is known.
An unknown native/host-operation outcome may keep settlement pending; no timeout
turns a missing proof into permission for conflicting work. The existing native
process-group containment assumptions remain unchanged.

## Reproducible checks

Run from the checkpoint checkout, with its own Cargo target directory and the
repository-pinned Bun version. Build the SDK before installing the file-dependent
plugin-host package.

```sh
cargo test -p rw-plugin-protocol -p rw-providers -p rw-ext -p rw-core
cargo clippy -p rw-plugin-protocol -p rw-providers -p rw-ext -p rw-core -p rw-runtime --all-targets --all-features -- -D warnings
cargo run -p rw-plugin-protocol --bin rw-plugin-protocol-codegen -- --check
cargo fmt --all --check
cargo check --manifest-path fuzz/Cargo.toml --bin plugin_rpc --locked
python3 scripts/check-ownership.py
node --test packages/docs-site/test/site.test.mjs
```

SDK: `bun run typecheck`, `bun test`, `bun run build` in `packages/plugin-sdk`.
Then install and typecheck/test `packages/plugin-host`.

Regressions cover a full unread data window with unrelated RPC replies, terminal
ordering, a slow physical SDK output pipe, shutdown with exhausted data credit,
real parent/child mutation after stream abandonment, forced outer-future drop,
self-retiring bounded ownership, destructor panic, encoded-buffer saturation,
original numeric/escaping byte refunds, and repeated native Bun-to-Rust credit
windows. No native Linux, performance calibration, hosted CI, or release/soak
qualification is claimed by these local checks.


Local result: the combined Rust run passed 606 tests (three pre-existing ignored
soak tests), including 310 core, 123 extension, and 106 provider unit tests.
All-target/all-feature scoped clippy, codegen consistency, formatting, fuzz-bin
compilation, and ownership checks passed. Pinned Bun passed 67 SDK tests and four
plugin-host tests, both typechecks, and the SDK build. Five documentation-site
projection tests passed. After the final Arc ownership simplification, the three
provider settlement regressions were rerun.

## Review correction: cancellation ownership

The SDK provider handler no longer races its invocation against cancellation.
An ignored abort retains its invocation and provider admission until the handler
and asynchronous iterator cleanup actually settle. The host's immutable deadline
and owned process teardown remain authoritative for an uncooperative plugin.
Cancellation errors are emitted only after that local invocation has exited.

The production duplex regression holds both the handler and its `finally` cleanup
behind independent gates. Neither cancellation nor shutdown may complete before
both gates release. It fails against the preceding implementation because the
cancelled RPC response appears before the first gate opens. After the fix all 71
SDK tests, typecheck, clean-package validation and build pass. Credit-starved
provider cancellation still settles and preserves the cancellation error code.
