# Runtime semantic modules

The runtime entrypoint is now 80 lines. Its production code lives in named domains for composition, persistence, checkpoints, providers, command execution, extension discovery, hooks, network services, subagents, and presentation. The largest new production file is `headless_session.rs` at 1,160 lines. Existing callers use the same module API; no forwarding functions or compatibility adapters were added.

The former inline test module is now 18 behavior modules and a 1,044-line shared fixture owner. Each behavior module has explicit imports. The largest test module is 1,076 lines.

The parsed Rust comparison against `6ea7e82` preserves all 538 function bodies from the remaining parent. Earlier durable and stable-domain moves have separate receipts. Changes in this split are module placement, imports, and the visibility needed by sibling owners. The tool composition entry in the ownership manifest points to its actual implementation.

Validation:

- Runtime unit suite: 264 passed, one existing manual diagnostic ignored.
- Runtime all-target/all-feature strict Clippy passed.
- Parsed function-body comparison and maximum 1,500-line check passed for the split.
- Thirteen network-boundary checker tests and repository ownership checks passed.
- The network checker regression now verifies the runtime exports and actual hosted composition. Its former assertion requiring more than 100,000 source bytes contradicted semantic module splitting.

The repository-wide cap still reports four core and five TUI files. The production raw-network check also exposes older test-module classification and Linux sandbox-owner path gaps; those checks are being corrected without exempting production code. Runtime presentation remains in the explicitly named `cli_output` module until the A08 caller migration; this split does not claim A08 completion.
