# A04 runtime transcript service checkpoint

This checkpoint connects the existing semantic projector to the production
`RuntimeSessionFactory` read-query methods. It does **not** yet replace the TUI's
historical reducer or the CLI's lifetime replay array. Those caller migrations
remain the next unit; the client cache prototype is separate uncommitted work.

## Ownership and bounds

`TranscriptReader` shares the factory's acknowledged `JournalReads` owner. It
retains at most eight independently locked projectors with their existing 4 MiB
redb caches. A read captures its journal prefix after entering the session owner.
Projector opening, advancement and eviction flushes happen outside the registry
lock. Busy sessions reject admission instead of accumulating queued work.

Each request advances at most four existing 64-event projection batches, then
returns `CatchingUp`. Source records retain their independent 16 MiB journal line
allowance; the semantic mutation batch remains 1 MiB. An already caught-up index
serves pages without replaying source records. First/after/ordinal reads seek
forward; latest/before seek backward under both row and byte bounds, so a small
page keeps the actual requested tail. Page accounting measures each item once
without constructing an extra encoded page.

Responses identify the exact applied prefix. Late tool outcomes invalidate their
stable row even when structural generation is unchanged. Rewinds reject old
ordinal generations and resolve removed stable anchors to a surviving row.
Only a typed incompatible projection-version error triggers rebuilding derived
state; storage corruption does not become a silent rebuild fallback.

Canonical content bodies share a 16 MiB / 32-document cache. A document is built
once from its closed source selector, and each response copies only a bounded
UTF-8 slice. Retained capacities and cache metadata are charged. Pinned `Arc`
readers prevent eviction until released. Eight blocking-worker permits bound
in-flight projection/document work separately from retained caches; permits move
into the worker closure, so cancelling an awaiting request cannot release a still
running worker's admission.

The extracted `HostReadChannel` is the actual owner used by `EngineHost`, and can
serve a historical reader without a fake session factory or mutation capability.
Its global/client/count/byte/identity leases remain attached to encoded response
bytes through their last clone. Controls are rejected before calling a backend.
Read and control payload hashes now stream directly into BLAKE3 rather than first
allocating a serialized command copy.

## Verification

Local private cargo target, unchanged budgets:

- Core host suite: 44 passed. Includes standalone mutation rejection, authenticated
  identity replacement, cancellation settlement, retained-body clone admission,
  and conflicting request reuse under ledger eviction pressure.
- Runtime transcript service: 11 passed. Includes 300-event bounded catch-up,
  first/middle/tail paging, byte-limited exclusive backward reads, late final row
  invalidation, rewind anchor recovery, incompatible-version rebuild, UTF-8 body
  chunks with one document build, aggregate pinned-document admission, blocked
  workers surviving caller cancellation, and offline access without an actor.
- Store transcript index: 13 passed; one pre-existing explicitly ignored
  performance probe was not run. The added descending-page test verifies actual
  tail selection under independent row and byte limits.
- Strict all-target clippy: `rw-core`, `rw-runtime`, `rw-store`.

The host regression initially exposed a test that changed private config after
construction. It now constructs its one-entry ledger through the public host
constructor, matching real runtime ownership. No production limit was relaxed.

These are bounded functional and ownership checks, not a release performance or
long-soak qualification. Live SSE recovery and aggregate TUI adoption remain open.
