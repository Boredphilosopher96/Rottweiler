# Direct host read channel and owner extraction

This checkpoint adopts direct replies for existing host queries. It does not
claim transcript-service, viewport, or aggregate client-cache adoption.

## Contracts exercised

- `EngineHost::dispatch` classifies reads and controls through `ClientCommand`.
  Existing query consumers now decode `CommandReply::Read`; no old HTTP outcome
  adapter remains. Mutation acknowledgements retain their existing channel.
- Query payloads do not enter SSE or the mutation result cache. A retained
  request ID carries its authenticated client identity and payload hash; an
  identical read re-queries current state, while conflicting reuse fails. Active
  identity remains charged and non-evictable until every response body clone
  drops, even when unrelated requests churn the retained ledger.
- Read admission is global 8/client 2, independent of 64 control executions.
  The 8 MiB general encoded envelope accommodates existing image previews.
  Each query reserves its envelope from 32 MiB aggregate encoded storage, then
  releases unused reservation after encoding. `Bytes::from_owner` retains both
  concurrency and actual allocation charge until every HTTP-body clone drops.
  Domain page limits remain smaller. This bounds encoded buffers; decoded query
  results and client caches have separate owners and limits.
- The TUI decodes a bounded amortized byte buffer and uses source-generated AJV
  union dispatch, including nested event unions. It rejects malformed known
  payloads, non-connection events, wrong protocol versions, and mismatched
  authenticated request/client IDs and bound session IDs before forwarding data. Late responses from
  an old runtime session generation cannot reach its replacement.
- Runtime host composition, fork persistence, query services, workspace access
  and Git commands have separate source modules. Core contracts, control
  dispatch, command execution, lifecycle, event forwarding and auth completion
  have separate owners. Protocol commands/events/shared values and feature tests
  are split, and the ownership manifest follows the definitions.

## Regression coverage

New host tests exercise direct/no-SSE replies, same-ID fresh reads, request-ID
conflicts, per-client/global admission, byte reservations, clone lifetime,
small-buffer reservation release, active identity under ledger eviction pressure,
invalid protocol/request metadata, and control progress while reads are full.
Encoding failure uses a static typed rejection body, with an equivalence test
against the Rust reply schema; fallback never recursively invokes encoding.
Transport tests exercise malformed/foreign replies, one-byte multibyte UTF-8
fragmentation, actual-byte overflow cancellation and malformed UTF-8.

A broad runtime run exposed two unrelated verification weaknesses. The real
sandboxed toolchain fixture inherited a private `CARGO_TARGET_DIR` outside its
workspace. Its configured linter now explicitly removes that inherited setting.
The Git query helper detached a blocking stdout reader and gave it a 50 ms
scheduling window after process exit. It now owns a nonblocking pipe alongside
the child, observes the existing command and drain deadlines, and closes the
pipe/kills and reaps its process group on every exit. No detached reader can
outlive the request. The descendant-held-stdout regression repeats four times
under its original three-second assertion; no deadline was increased.

## Still required

The existing control result cache still needs shared byte ownership and bounded
aggregate eviction independent of completion delivery. The runtime transcript
service, canonical body cache, virtualized first/middle/latest history, child
views, aggregate client cache, stale-generation rejection and scroll-anchor
recovery remain the next A04/A09/A16 unit. Initial live-state recovery remains
coordinated with A02. These are not covered by the direct-query tests above.

## Verification checkpoint

- Strict `cargo clippy -p rw-core -p rw-runtime -p rw-cli -p rw-types
  --all-targets -- -D warnings`: passed.
- Final core host tests: 41 passed, including all new admission/identity cases.
- TUI full suite before the final session-correlation guard: 553 tests passed,
  21 snapshots. Final typecheck and six focused transport tests passed afterward.
- Final full runtime suite with explicit pinned Bun PATH: 218 passed, 1 ignored
  in 31.91 seconds, including all process cleanup and sandboxed toolchain cases.
- CLI unit suite: 150 passed. Types unit suite: 15 passed.
- Ownership manifest, Rust codegen and generated AJV validator checks passed.
- Source-level toolchain isolation was separately verified with inherited
  `CARGO_TARGET_DIR` deliberately set to the private worktree target: passed.

All Rust commands used the private `/tmp/rw-client-bounds/target` target. One
broad runtime run inadvertently inherited system Bun 1.4.0 and failed a real
TypeScript plugin initialize RPC (217 passed/1 failed/1 ignored); the unrelated
codegen version check also correctly rejected that environment. The same three
sandboxed plugin shapes passed with explicit pinned Bun 1.3.14 in 24.08 seconds,
without changing initialization deadlines. This does not establish whether Bun
version or concurrent load caused the first timeout; that evidence was handed
to the extension lifecycle owner for the recurring-fragility investigation.
These local results do not claim hosted performance qualification.
