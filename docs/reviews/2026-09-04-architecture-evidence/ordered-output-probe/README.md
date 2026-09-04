# Ordered output coordinator probe

This reproduces a permit starvation state in the production ordered-output algorithm. It is an **extracted algorithm test, not a complete engine integration test**.

`src/main.rs` copies `OrderedOutputState`, `BoundedOutputChunk`, and `OrderedOutputCoordinator` verbatim from `crates/rw-core/src/engine/turn/mod.rs` (lines 1793–1883 as reviewed on 2026-09-04). Lightweight event, output, error, and identity-redactor stubs replace surrounding engine dependencies. The capacity is the production value of 32 from `crates/rw-core/src/engine/mod.rs:165`.

Run from the repository root, keeping build artifacts outside the checkout:

```sh
CARGO_TARGET_DIR=/tmp/rw-ordered-output-probe-target cargo run --offline --quiet --manifest-path docs/reviews/2026-09-04-architecture-evidence/ordered-output-probe/Cargo.toml
```

The probe emits 32 chunks for a later tool at index 1, then tries to emit for the current tool at index 0. It asserts that no permits remain, that the actor queue is empty, and that current-tool output remains blocked for 100 ms. All permits are retained in the later-tool buffer. No consumer can release one. In the production scheduler, advancing that buffer requires the earlier tool to finish, which it cannot do while awaiting output. Cancellation can end the operation but does not provide normal progress.

Expected output:

```text
REPRODUCED: 32 later-tool chunks exhaust global permits; first tool output times out; actor queue empty, so no consumer can release permits.
```

The timeout bounds the probe; it does not imply this is merely a 100 ms slowdown. Add a full engine regression before changing the scheduler, using a delayed first read-only tool and a second tool that fills the ordered buffer. Candidate designs include capacity reserved for the active tool or separate per-tool bounded spools. Preserve canonical durable result ordering while proving progress under saturation.
