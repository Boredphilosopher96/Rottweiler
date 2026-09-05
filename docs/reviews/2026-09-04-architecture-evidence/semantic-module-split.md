# Extension and provider module split

The ten assigned oversized owners now fit the 1,500-line handwritten-file cap.
Production boundaries are discovery filesystem access and Markdown parsing;
plugin launch authority, incoming transport, and registry adapters; provider
activation, runtime routing, catalog projection, and native search; MCP service
and command composition; development generations; OAuth; and fixture redaction.
Tests are separate behavior suites, not numbered source fragments.

The public APIs and runtime policies stay unchanged. Existing ownership checks
follow the moved definitions; separate incoming-HTTP and catalog-limit checks
cover their new files. No runtime budget or test assertion was relaxed.

## Verification

Against pre-split checkpoint `5baf21a`:

- All-target, all-feature Clippy passes for `rw-ext`, `rw-providers`, `rw-core`,
  and `rw-runtime`, with warnings denied.
- The same four crates' all-target, all-feature tests pass: core 316 unit tests
  and 45 integration tests; extension 133; provider 106 unit and 18 integration
  tests; runtime 218. The pre-existing paid-provider cases and manual long-session benchmark
  remain ignored; these cases remain explicit in Cargo output.
- Native plugin tests use Bun 1.3.14, including actual sandbox launch, SDK duplex
  behavior, shutdown settlement, and repeated source-plugin shapes.
- `cargo fmt --all --check`, `git diff --check`, and
  `python3 scripts/check-ownership.py` pass.

The adjacent `verify-semantic-module-moves.py` checks 1,173 function bodies and
all ten owners' resulting file sizes. It ignores comments, whitespace, and
formatter commas, preserves string token bytes, and accepts exactly one
recorded before/after hash for Rustfmt removing a redundant closure block in a
test. It is a structural move check, not a general semantic-equivalence proof.
A separate literal inventory comparison found no missing original string,
raw-string, or character literals. Extraction initially changed indentation in
three multiline fixtures; their exact original bytes were restored before the
regression run.

To reproduce the structural check at this checkpoint:

```sh
python3 -m venv /tmp/rw-module-audit
/tmp/rw-module-audit/bin/pip install tree-sitter==0.25.2 tree-sitter-rust==0.24.2
/tmp/rw-module-audit/bin/python docs/reviews/2026-09-04-architecture-evidence/verify-semantic-module-moves.py --before 5baf21a --after HEAD
```

Run the check against the split commit rather than a later feature commit:
subsequent intentional behavior changes should fail the body comparison.
