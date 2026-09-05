# Root module splits

Baseline: `b4dcd5e`. Five handwritten production/test families were split by responsibility without changing function bodies or fixture bytes.

| Original owner | Extracted responsibilities | Preserved functions | Largest resulting file |
| --- | --- | ---: | ---: |
| `rw-core/permission.rs` | Invocation identity, rule matching, durable project approvals, policy and persistence tests | 146 | 1,185 lines |
| `rw-core/orchestration.rs` | Child lifecycle, policy/result bounds, tool interface, actor/worktree factories, lifecycle/tool tests | 138 | 1,037 lines |
| `rw-store/config.rs` | Effective values, loading, layered overrides, editing/persistence, validation, behavior tests | 135 | 1,205 lines |
| `rw-tools/worktree.rs` | Explicit artifact application, Git process I/O, validation, lifecycle tests | 100 | 1,030 lines |
| `rw-tools/bash.rs` | Safety classification, execution leases, recordings, native child ownership, watchdog, output, process groups, behavior tests | 182 | 730 lines |

Module entrypoints retain public domain types and exports. Internal imports identify their actual owner. Visibility was widened only within the containing module where another child or a test needs the definition. Subprocess helper tests retain their exact names so real child-process probes still execute.

The ownership checker now applies each Rust shadow rule to the named file and its child-module directory. Its regression test places a forbidden definition in a nested module and confirms rejection. The UTF-8 field-audit assertion now checks the output implementation and its test at their new locations.

## Reproduce the move proof

Install the two parser packages listed in the verifier's docstring into a temporary environment, then run from the repository:

```sh
python3 docs/reviews/2026-09-04-architecture-evidence/verify-semantic-module-moves.py \
  --before b4dcd5e \
  --roots crates/rw-core/src/permission.rs crates/rw-core/src/orchestration.rs \
          crates/rw-store/src/config.rs crates/rw-tools/src/worktree.rs \
          crates/rw-tools/src/bash.rs
```

The verifier parses every original and moved function, compares body-token hashes with exact literal bytes, and checks each resulting file's physical line count. All 701 functions match. This proves the move at this revision; later intentional behavior changes should use their own tests.

## Verification

- `cargo test -p rw-core -p rw-store -p rw-tools` passed, including permission authority, worktree isolation, configuration persistence, real Bash panic/caller-drop/process-tree settlement, recordings, and UTF-8 output tests.
- Ownership and field-audit contracts passed.
- Ownership and build-hygiene Python suites: 21 tests passed.
- `cargo clippy -p rw-core -p rw-store -p rw-tools --all-targets --all-features -- -D warnings` passed.

The repository-wide line gate still rejects the remaining engine/runtime and TUI files. Those files are being split with their owning architectural changes; no handwritten exemption was added.
