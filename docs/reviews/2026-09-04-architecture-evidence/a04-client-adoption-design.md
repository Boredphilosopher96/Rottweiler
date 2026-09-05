# Transcript client adoption contract

This is the accepted implementation contract. The native adoption checkpoint is
recorded separately in `a04-native-history-adoption.md`, including the remaining
legacy live-state/cache work. ADR-030 and ADR-033 own the semantic row and
authenticated read channel decisions.

## Ownership

`history/cache.ts` is the single owner of cached semantic items, child-session
pages, artifact descriptors, and fetched document chunks. It admits at most
16 MiB, 2,048 entries and eight session views in total. Charges include retained
string storage and fixed descriptor/container overhead; a reply is inspected
once at admission, never serialized again on each render. There is no independent
per-child allowance that can multiply the aggregate cap. Explicit viewer leases
pin a bounded working set; insertion evicts least-recently-used unpinned entries
or fails without partially publishing a page. Drafts and pending approvals are
active interaction state, not disposable history cache entries.

`history/controller.ts` owns outstanding read identities, session generation,
exact applied transcript view, sparse ordinal ranges and the viewport anchor.
A response is accepted only for its current request/session generation. A changed
structural generation discards ordinal assumptions; changed item revisions
invalidate the affected rows even when ordering did not change. Around(item)
recovers a removed anchor using the server's surviving replacement. Eviction
removes payload ownership and marks a gap; navigating that gap issues another
bounded read rather than rendering an empty historical interval.

`history/viewport.ts` owns stable item ID plus visual-row offset, following-latest
state, width-dependent measurements and the bounded mounted window. The native
scroll box contains only a sliding physical window. A separate ordinal scrollbar
maps a thumb seek to AtOrdinal with the current generation. This avoids converting
a u64 lifetime row count into an enormous native spacer or retaining one height
entry for every historical item. Home/End use First/Latest; ordinary wheel and
page movements retain a stable item anchor while adjacent ranges are fetched.
Only mounted items and one overscan window are measured. Width changes invalidate
those measurements without scanning the entire cache or journal.

`components/transcript-row.ts` consumes the semantic union directly. It does not
synthesize provider IR to reuse the old turn-card reducer. Conversation, tool,
command, shell and child rows each have one visible identity. A separate live-tail
owner renders streaming text/reasoning and active tool state. Host invocation IDs
join tool overlays to semantic rows; once the applied prefix includes a live
revision, the transient duplicate is retired. Raw canonical IR remains on the
engine side for provider continuation and audit/export.

`history/document.ts` reads complete content in bounded UTF-8 chunks and admits
those chunks to the same cache. Opening a large result never assembles its entire
body or mounts a native widget containing lifetime output. Source references remain
closed typed selectors scoped to an exact session/prefix. Clipboard actions must
state their scope and reject an oversized whole-document operation rather than
silently copying only a truncated preview.

## Runtime service

The runtime transcript service consumes `Arc<JournalReads>`. It retains at most
eight independently locked projectors, each using the existing 4 MiB redb cache.
The registry lock covers lookup/admission only; index opening, projection, I/O and
closing an evicted database happen outside it. A session read owns one projector
operation and captures its journal view after admission, so an older queued view
cannot move a newer projection backward. Work advances a fixed number of existing
bounded projection transactions, then returns CatchingUp if necessary. A dropped
request cannot start an unlimited detached catch-up loop.

Page requests use the existing bounded row index, exact prefix, generation and
revision invalidations. Normal first/middle/latest reads use indexed seeks, not
raw event replay or SQL OFFSET scans. Canonical content resolution reads one
referenced durable event under the supplied verified view and constructs a
bounded document once; repeated chunk reads reuse its byte-charged owner.

## Deletions and module boundaries

Adoption removes the recent-256 transcript array as the historical authority,
recent-16 mounted history crop, full-IR child replay cache, and full-output viewer
copies. The reducer retains current engine/control state and bounded live overlays.
Timeline and rewind affordances must use semantic item/turn metadata, not retain a
second hidden lifetime array to preserve old callers.

The existing oversized component file separates reasoning, tool blocks, child
panels, live tail, semantic row and viewport owners. Panels separate review,
interaction, context and status. Reducer command-result parsing and child/tool
projection helpers move to their own domain owners; event dispatch remains small
and exhaustive. App becomes composition and route coordination; history loading
and interaction controllers own their state. Tests split by those same features,
not numbered source fragments. Every handwritten owner remains below 1,500 lines.

## Required proofs

- First, middle, last, wheel, page and thumb navigation over a long fixture restore
  evicted rows and preserve an item anchor across append, resize and rewind.
- Same-generation late tool/diff updates invalidate cached rows; stale in-flight
  responses cannot resurrect an old view or a removed item.
- Parent history, several children and large document chunks compete for one
  measured aggregate cache budget. Repeated revisits plateau retained bytes and
  mounted native cards; old views do not retain evicted payloads.
- Full-content inspection reads UTF-8 boundaries correctly and does not flatten
  the complete document per chunk or frame.
- Streaming work and mounted output retain the existing process-CPU budgets;
  local bounded counters attribute decode, queue age, reduction and render time
  without recording content.
- Real renderer capture/destroy/recreate restores the logical anchor and active
  bounded interaction; unsafe handoffs continue to defer renderer recycling.
