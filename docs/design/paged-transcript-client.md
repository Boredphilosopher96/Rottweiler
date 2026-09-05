# Paged transcript and client history ownership

The terminal reads semantic transcript pages from the engine and renders a bounded window of rows. Canonical journal events are authoritative. The transcript index is a rebuildable display projection; provider context and audit history have separate owners.

## Ownership

| Owner | Responsibility |
| --- | --- |
| `rw-types/src/transcript.rs` | Page, content, identity, revision and invalidation contracts; generated client types and validators. |
| `rw-core/src/transcript.rs` and `transcript/` | Conversation display blocks, tool and child associations, turn summaries, command and shell records, and rewind interpretation. |
| `rw-store/src/session/transcript_index.rs` | Indexed ordering, atomic row/checkpoint publication, journal identity binding and derived storage. |
| `rw-runtime/src/transcript_service.rs` | Bounded source reads, projection catch-up and content lookup outside the session actor. |
| `packages/tui/src/history/` | Read capability, session view selection, request cancellation, charged cache leases and document pages. |
| `packages/tui/src/components/transcript.ts` | Native row mounting, expansion, selection, scrolling and live activity. |

## Read contract

`HistoryReader` exposes only `page(sessionId, read, signal)` and `content(sessionId, read, signal)`. Live and historical views use this capability. Reads travel through the authenticated command connection and return typed `CommandReply` bodies. They do not place page bodies in the mutation acknowledgement ledger or durable SSE stream.

A `TranscriptView` identifies the session, projection version, structural generation and exact applied journal prefix (`through`, `digest`). A page includes row ordinals, total item count, an anchor result and bounded invalidation information. Ordinals describe logical display order; they are distinct from durable event sequence IDs.

Positions support first, latest, before, after, around and ordinal lookup. An ordinal lookup includes its expected generation. A changed ordering returns `OrderingChanged`; an incomplete projection returns `CatchingUp`. Neither result exposes an incomplete row set as a complete transcript. `Around` reports a replacement when the requested item no longer exists.

Each row has stable identity and a revision sequence. Tool lifecycle events bind to a host-owned invocation identity. Late completion and diff events update that row rather than creating duplicate tool output from provider IR. `TurnSummary` is sourced from `TurnFinished`; provider accounting receipts do not add a second displayed total.

The index serves the effective transcript at its applied prefix. Rewinds change structural generation. Item revisions and invalidations also cover changes that preserve ordering. The client rejects superseded requests and regressing views. Immutable journal history remains independently available for audit and export.

`SessionHistoryReady` announces history availability. It does not advance the client's durable cursor or establish that live control-state replay has completed. Loading display rows therefore does not authorize skipping approval, driver or active-operation events.

## Content and cache bounds

The TUI requests at most 32 items and 256 KiB per transcript page. Historical rows contain bounded previews and typed source references. Content lookup accepts a semantic selector and exact source identity, not a filesystem path or arbitrary JSON pointer.

`DocumentController` reads 4 KiB UTF-8 chunks and limits the referenced document to 16 MiB. It retains bounded continuation offsets and displays one chunk at a time; it does not concatenate the full document into a Markdown renderable. Transcript pages and document chunks share `ClientCache`.

The default cache admits 16 MiB of charged values and 2,048 entries. Charges account for retained strings, structured values, keys and entry overhead. A mounted reader holds a lease on its exact revision. Removing or replacing a resident entry does not release its charge until its final reader releases it. Admission evicts unpinned entries in least-recently-used order and rejects a value that cannot fit beside pinned readers.

`HistoryController` retains at most eight session views and 32 page-range descriptors per session. Evicted content can be fetched again through its durable source. Child transcript pages use the same cache. The live reducer does not retain conversation or command bodies. It owns an activity flag and one bounded foreground-shell preview; command cards format their own bounded semantic source previews. Historical formatting never borrows context or cost state from a different turn. Complete structured command values redact private fields, and incomplete structured previews expose the content action without interpreting partial JSON. These limits describe the history allocation owner; they are not a process-RSS ceiling or a bound on every live-state collection.

## Native viewport

The transcript mounts at most 16 historical row cards from the active page. Scrolling moves that window and requests adjoining pages at its boundaries. A logical ordinal scrollbar permits distant navigation without creating a placeholder or measured-height record for every lifetime row.

Before replacing the visible window, the renderer captures the first visible item ID and its offset from the viewport. It restores that offset after OpenTUI layout, including width changes. Following latest is explicit state: newly available rows follow the tail only while that state is enabled. A removed anchor uses the engine's replacement result.

Cards keep bounded previews and use OpenTUI for text selection, terminal cells, input and damage tracking. Unmounted historical cards are destroyed. Live reasoning and tool activity have a separate tail; durable row arrival transfers matching selection and expansion identity. A live delta does not rebuild the historical projection.

Renderer handoff is governed by `AppClientState` and its private size limit. Interactions that cannot be safely captured defer replacement. The handoff stores follow-tail intent or the visible stable source item and its signed viewport offset. A replacement resolves that item through an `around` read before restoring layout, including the index-provided replacement when rewind removed the source item. Unresolved navigation defers replacement; physical window scroll offsets are not durable history positions.

## Local diagnostics

`ROTTWEILER_CLIENT_TIMINGS=1` enables fixed-size stage counters for event/reply decoding, reply validation, reduction, presentation, history admission/update/layout and queue age. The counters contain no payloads or session identifiers and are emitted on renderer teardown. Disabled call sites do not read a clock.

Measurements are wall-clock durations. Nested stages are not additive, and asynchronous syntax parsing or terminal I/O may run outside the measured call. Process CPU and end-to-end input latency remain separate measurements.

See [Terminal workspace](terminal-workspace.md) for interaction and visual ownership.

## Client read admission

The HTTP transport owns one FIFO for every direct read, including history, catalogs, workspace inspection, and service views. `MAX_CLIENT_READS` is generated from the Rust protocol owner and limits active reads through complete reply-body consumption and validation. Actor controls and mutations bypass this FIFO.

Active and waiting requests share a 32-entry, 1 MiB retained-request allowance. Admission measures a bounded JSON traversal before cloning the request; the charge covers the immutable snapshot and worst-case JSON escaping and encoding. Caller mutation cannot change an admitted request. Queue overflow rejects the request without dispatch. Aborting a waiting request removes it immediately; an active request retains admission until its HTTP operation settles. Runtime stop and session changes abort their owned reads.

Opt-in local diagnostics record `read_queue_age` in fixed counters and histogram buckets. Measurements contain no command names, session IDs, or payloads.

## Timeline actions

The conversation timeline reads semantic pages through the same charged cache as the transcript. Each page exposes older/newer navigation. Selecting a committed user source retains its exact view prefix and source identity; a source can be browsed even when the actor cannot currently rewind it.

Edit and retry read the complete first message-text block through its typed content selector. The draft owner reserves chunk, join and editable-text capacity before continuation reads. Capacity refusal or a changed content source stops the operation before a history mutation is dispatched. Original attachments are explicitly excluded from these text actions. A failed retry or edit completion merges the retained source text with any newly typed draft instead of overwriting it.

A source rewind includes the expected committed prefix, committed user source sequence, turn identity and a before/through boundary position. The actor checks these inputs inside serialized mutation admission. Reused turn numbers and stale client pages cannot select another effective source. The client applies edit/retry follow-up only after the corresponding durable rewind event carries the exact request identity. A before action without an earlier completed workspace boundary is unavailable.

Timeline handoff restores the selected source through an around read. Unresolved reads and navigation controls without a selected source defer renderer replacement. Independent history readers have distinct cache key namespaces; disposing one releases only its pages and leases.

## Task state

Task state has one authoritative typed snapshot, committed independently from transformed tool presentation. The client retains no historical task-output checkpoints. A rewind immediately invalidates the displayed snapshot and records its physical sequence as the minimum acceptable read prefix. A late query cannot replace a newer live state commit.

`GetTodos` uses the authenticated read channel in live and historical views. Each query advances the mode-independent index by a bounded number of transactions and returns either an exact snapshot or explicit catch-up progress. The client owns one pending task request and one catch-up timer; session changes and renderer destruction retire that timer. Task identities, item count, per-field UTF-8 bytes and aggregate text bytes are validated from the same source schema in Rust and generated client validators.

## Asynchronous input

The draft owner admits one outstanding image, external-editor or history-text read. Its reservation covers the eventual retained draft and survives destination cancellation until the read settles. Submission and renderer replacement wait for accepted input reads. Image completion adds its attachment without moving the current cursor or focus; external-editor completion merges a newer draft instead of overwriting it.

Ordinary text paste is a synchronous editor operation at the initiating selection. The platform classifies explicit local image paths into deferred read capabilities before I/O; a recognized image path attaches an image or reports a read failure. Image files are descriptor-checked and bounded to 5 MiB. External-editor input/output is bounded to 2 MiB, and output is read from a checked regular-file descriptor.

## Tool surfaces

A completed tool row stores a compact title and an invocation-bound canonical source reference. Opening that reference uses the existing content read capability and exact journal view. The full declarative surface is collected from bounded UTF-8 pages, validated from its generated schema, and prepared once into native text, badge, list and table fields. Neither row paging nor frame rendering executes plugin code or repeats selector traversal.

The document owner presents one full surface at a time. It reserves collection and decode credit in the shared history cache before reading, then transfers that credit to the validated surface and prepared field strings. Cancellation retains the reservation until the outstanding read settles. Render nodes borrow the pinned model's strings and clear their content before destruction; replacement releases the previous cache lease after its nodes stop using it. Theme changes rebuild nodes against the same pinned source without another read.
