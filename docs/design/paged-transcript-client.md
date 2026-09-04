# Paged transcript and bounded client ownership

Status: accepted A04 contract, 2026-09-04; implementation in progress. This document does not itself change the wire protocol. A02 owns the raw journal and pinned read views; A04 owns the semantic transcript projection and its client.

- [x] Ground existing journal, child replay, reducer and native transcript paths.
- [x] Compare client event paging with an engine-owned semantic index.
- [x] Agree cross-boundary contract and ownership.
- [ ] Implement and verify each complete vertical unit.
- [ ] Revisit the design if implementation requires parallel authorities.

## User-visible behavior

Opening a session shows its latest transcript page and live activity. Scrolling upward retrieves older messages. Home, End and a source-item jump can reach earliest, latest and specific history without downloading intervening bodies. New activity does not move a reader away from the message they were reading. Evicted pages can be fetched again. Reconnect and renderer replacement restore the selected item and position within it, rather than reusing an obsolete absolute scroll offset.

Historical cards contain bounded display content. Large text, reasoning, images, tool output and diffs carry engine-owned content references. Opening complete content uses a bounded document reader; it never requires mounting the entire body as one Markdown renderable.

## Existing contracts and the decision

The existing `load_bounded_subagent_replay` returns raw tail/after event pages with durable sequence cursors. The top-level reducer retains 256 transcript entries; `TranscriptRenderable` mounts the latest 16 irrespective of scroll position. Tool details live separately in the reducer, while rewinds remove prior logical rows. Replaying an arbitrary raw event page cannot reconstruct those cross-page effects.

Two shapes were considered:

1. Client-owned pages of raw events, with checkpoints before every page. This keeps the current reducer but couples history reads to all control-state reduction, duplicates checkpoints across clients and makes arbitrary jumps depend on a valid earlier checkpoint. Tool finalization and rewind semantics would still require a separate authoritative index.
2. Engine-owned, versioned semantic rows indexed against a pinned journal prefix. Clients fetch ready-to-display semantic content and independently format it. Rewind and tool association are evaluated once by the engine projection. The index is derived and rebuildable, with no authority beyond its journal prefix.

Choose the second. Core owns semantic row identity, ordering, associations and rewind interpretation. Store owns bounded opaque index/checkpoint persistence and safe indexed reads. Runtime schedules bounded blocking work. The TUI owns rendering, sizing, expansion, selection and viewport position. No terminal dimensions, colors or Markdown presentation enter core.

## Protocol contract

The names below are a sketch. Rust `rw-types` owns final wire types, generated schemas, validators and fixtures. Collection/wire limits also have one Rust owner and generated client projections.

```text
TranscriptView = {
  session_id,
  projection_version,
  projection_generation,     // changes when prior logical rows are invalidated
  journal_prefix            // A02's exact next_sequence + digest identity
}

TranscriptItemId = source durable sequence + semantic discriminator
TranscriptOrdinal = logical row position in this view, not a durable sequence

ReadTranscript {
  meta,
  session_id,
  known_view?: TranscriptView, // advisory revision for invalidation, not an old snapshot
  position: Latest | First | Before(ItemId) | After(ItemId) | Around(ItemId),
  max_items,
  max_bytes
}

TranscriptPageReady {
  command_ack_meta,
  view,
  items: [TranscriptItem],
  first_ordinal,
  total_items,
  before_cursor?, after_cursor?,
  anchor: Exact(ItemId) | Replaced { requested, replacement?, reason },
  encoded_bytes
}

TranscriptItem = {
  id, ordinal, source_sequence, revision_sequence, agent_turn,
  content: Conversation | CommandResult | ShellResult,
  associated_tools, associated_subagents,
  content_references
}

ReadTranscriptContent {
  meta, view, content_ref,
  position: Start | Continue(opaque_content_cursor),
  max_bytes
}

TranscriptContentPageReady {
  command_ack_meta, content_ref, content_kind,
  content, next_cursor?, completeness
}
```

`Conversation` uses existing provider-neutral IR variants with bounded inline text and reference descriptors for larger bodies. Command results retain their existing semantic source payload; the TUI continues to own its structured presentation. Tool identity, arguments, completion, timing and body references are included with the historical row so a missing tool-cache entry cannot turn an old tool card into an empty placeholder. Each semantic record has an explicit projection version.

A content reference names a session, exact journal prefix/source identity and a closed semantic selector such as turn block, tool output or command output. It never contains a host filesystem path or arbitrary JSON pointer. The engine validates session access and content identity on every read. Text continuation boundaries preserve UTF-8; structured values and images use bounded typed chunks. Content is fetched through the authenticated command/event channel in remote and local modes.

Initial limits to test: 64 items and 1 MiB per transcript response; 32 KiB inline preview per item; 64 KiB per content response; two outstanding history/content reads per connection with a shared host admission cap. Over-limit individual source bodies become references, not an error that makes history unreachable. Pages always make progress or return an explicit bounded error. Reads are connection-scoped, cancellable and do not advance the durable event cursor. They run outside the session actor's mutation path and journal writer lock.

The engine returns only the prefix through which its derived projection is complete. It must not claim the latest journal tail if the index has not applied it. A02 can reopen the exact raw prefix statelessly and seek referenced source records. Retaining a client view does not retain server descriptors or a historical semantic database version.

## Projection updates, mutation and recovery

Canonical durable events remain the sole source of truth. The projection applies committed conversation, command and shell items; associates tool/subagent state with source rows; and interprets rewind effects across the entire logical timeline. It stores bounded row previews and content references, not lifetime body copies. Index publication records its exact applied prefix only after all effects through that prefix are complete. A crash may leave the index behind; it must never make it appear ahead. Missing, corrupt or version-incompatible indexes rebuild incrementally from bounded A02 pages. Explicit rebuild cost is measured separately from normal indexed reads.

Ordinary appends preserve existing item identity and ordering. New live rows can join the visible latest window. A rewind changes the projection generation and invalidates affected cached ordering/associations. A pending response for another generation or replaced request cannot mutate the active view. Every response is internally consistent at its exact applied prefix. The service exposes the current effective transcript; it does not reopen old semantic snapshots. Immutable raw history remains available separately through the journal, audit and export. If an anchor was removed, resolve its source ordering to the nearest surviving predecessor or the first item and report the replacement.

A late tool completion, diff or association change increments the affected item revision even when structural generation stays unchanged. Page replies include bounded invalidations since the supplied view, or an explicit whole-cache invalidation if the change set exceeds its cap. Clients reject responses older than the accepted applied prefix or from a superseded request; a matching structural generation alone is not evidence that cached content is current.

Ordinary inserts append dense ordinals, updates address indexed item identity, and normal page reads seek an indexed ordinal without scanning or counting the historical prefix. Rewind preserves the existing logical rule: conversation and associated tool rows beyond the target turn disappear, while command and shell records remain. Restoring dense ordinals after that mutation may require work proportional to the affected suffix. Perform that work in bounded, cancellable transactions and publish the replacement generation only when complete. During rebuild, reads return an explicit progressing/retry result rather than claiming a partially rewritten index is complete. Measure total rewind/rebuild cost separately from ordinary indexed reads.

A04's transcript index is distinct from A02's bounded live recovery snapshot. Historical conversation/context recovery must reference journal/projection cursors, not serialize the entire row catalog inside a live snapshot. The initial implementation must explicitly retain existing live-state replay until a complete client recovery snapshot can replace it; a transcript page alone cannot authorize skipping control, permission, todo or active-operation events. Thus successful paging does not by itself prove constant-cost initial attachment.

## One aggregate cache owner

Introduce `ClientHistoryStore`, owned by the app rather than individual renderers. It admits immutable transcript pages, content chunks and historical child views under one charged budget. Proposed starting limits are 16 MiB total retained payload, 2,048 row descriptors and at most 8 retained session views. Entry charges include strings, inline structured previews and content chunks; reference-only descriptors have their own count cap. Response memory is additionally bounded by the two-read admission limit. These are application allocation bounds, not a claim that JavaScript RSS equals serialized bytes.

Viewport and immediate overscan pages are pinned while mounted. Evict least-recently-used unpinned pages/content first. If a requested page cannot fit beside pinned data, release the previous viewport after capturing its anchor and show a loading placeholder; do not silently exceed the cap. Empty descriptors record that a range is unloaded, not absent. Metadata itself is bounded: store sparse unloaded ranges and aggregate ordinal counts, not one placeholder per historical message.

Child history uses this same store, keyed by child session and view. An inactive child's heavy projection/content can be evicted and reloaded. Its editable draft belongs to `AppClientState`, outside the evictable history cache; cache pressure must never discard it. Only the active child needs detailed live progress. Inactive children retain bounded status summaries and a recovery cursor. Selection/fold metadata is separately capped and can outlive a page; loss of a cosmetic fold preference must never imply loss of durable content.

Current live operations, approvals and accepted input are not evictable artifacts. A16's live tool/text limits remain. An aggregate live-operation admission/overload policy must be coordinated with engine scheduling; historical cache eviction alone cannot bound arbitrarily many active operations.

## Viewport and rendering

`TranscriptViewport` owns a sparse measured-height index over logical row ranges. It computes visible items plus overscan from scroll position and mounts a bounded card pool. Unloaded/unmounted ranges use spacer heights with estimates; measured corrections preserve an item anchor. No renderer object or measured-height entry is retained per lifetime item.

The anchor is `{session_id, item_id, block_id?, row_offset, affinity}` where affinity distinguishes following latest from holding an item. Capture the first visible stable item before prepend, eviction, measurement change, width change or replacement. Restore that item's viewport offset after native layout settles. A renderer recycle stores this anchor in the existing bounded private handoff, then fetches around it before restoring focus. Absolute `scrollTop` is only a local layout value.

Native cards and syntax parsers receive bounded preview bodies. Larger content opens the bounded content reader, which uses the same cache and viewport machinery. Do not create an unbounded Markdown body inside a fixed-size scroll box. Keep the separate live tail and avoid recomputing historical projections for a live delta. OpenTUI remains responsible for cell rendering, damage tracking and input; the new code controls which cards exist.

## Delivery and acceptance

1. Core semantic projector and derived index, with cross-page tool/rewind tests, item revisions and exact applied-prefix identity checks. No client schema before this invariant is specified.
2. Rust-owned page/content commands, generated contracts/validation, bounded runtime admission and real authenticated transport tests. Parent and child use one semantic history service.
3. Aggregate client store and anchor/range model with deterministic eviction, stale-response, reconnect and content-paging tests.
4. Native viewport integration and handoff migration, removing the recent-tail-only rendering path. Migrate fixtures rather than preserving a shadow implementation.

Acceptance uses a real 10,000-item mixed transcript with asserted source bytes. Navigate first/middle/latest, evict and revisit pages, append while scrolled away, resize after measured heights change, jump to an item, reconnect, recycle and handle a rewind removing the anchor. Test large bodies, multiple children and repeated open/close cycles. Assert cache bytes/rows, in-flight bytes, mounted cards, per-frame visited rows and full historical reachability. Use native OpenTUI and real transport for the final path, including Tree-sitter in representative code/diff bodies. Preserve existing frame/input budgets and classify shared-host timing evidence honestly.

This follows ADR-001, ADR-002, ADR-015 and ADR-028, and uses A02's new ADR-029. ADR-030 formalizes the ownership and revision decision. It need not reverse the frontend/core split or canonical event authority.
