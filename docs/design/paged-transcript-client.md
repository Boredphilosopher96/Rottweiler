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

`SessionReader` exposes typed transcript pages, source content and exact task snapshots for an explicitly selected session. Live, child and historical views use this read-only capability. Transcript controllers borrow only its page/content methods. Reads travel through the authenticated command connection and return typed `CommandReply` bodies. They do not place page bodies in the mutation acknowledgement ledger or durable SSE stream.

A `TranscriptView` identifies the session, projection version, structural generation and exact applied journal prefix (`through`, `digest`). A page includes row ordinals, total item count, an anchor result and bounded invalidation information. Ordinals describe logical display order; they are distinct from durable event sequence IDs.

Positions support first, latest, before, after, around and ordinal lookup. An ordinal lookup includes its expected generation. A changed ordering returns `OrderingChanged`; an incomplete projection returns `CatchingUp`. Neither result exposes an incomplete row set as a complete transcript. `Around` reports a replacement when the requested item no longer exists.

Each row has stable identity and a revision sequence. Tool lifecycle events bind to a host-owned invocation identity. Live tool maps, streaming membership, selection, output viewers and approval notifications use that identity; provider call IDs remain correlation data for engine commands. Late completion and diff events update that row rather than creating duplicate tool output from provider IR. `TurnSummary` is sourced from `TurnFinished`; provider accounting receipts do not add a second displayed total.

The index serves the effective transcript at its applied prefix. Rewinds change structural generation. Item revisions and invalidations also cover changes that preserve ordering. The client rejects superseded requests and regressing views. Immutable journal history remains independently available for audit and export.

`SessionHistoryReady` announces history availability. It does not advance the client's durable cursor or establish that live control-state replay has completed. Loading display rows therefore does not authorize skipping approval, driver or active-operation events.

## Content and cache bounds

The TUI requests at most 32 items and 256 KiB per transcript page. Historical rows contain bounded previews and typed source references. Content lookup accepts a semantic selector and exact source identity, not a filesystem path or arbitrary JSON pointer.

`DocumentController` reads 4 KiB UTF-8 chunks and limits the referenced document to 16 MiB. It retains bounded continuation offsets and displays one chunk at a time; it does not concatenate the full document into a Markdown renderable. Transcript pages and document chunks share `ClientCache`.

The default cache admits 16 MiB of charged values and 2,048 entries. Charges account for retained strings, structured values, keys and entry overhead. A mounted reader holds a lease on its exact revision. Removing or replacing a resident entry does not release its charge until its final reader releases it. Admission evicts unpinned entries in least-recently-used order and rejects a value that cannot fit beside pinned readers.

`HistoryController` retains at most eight session views and 32 page-range descriptors per session. Evicted content can be fetched again through its durable source. Child transcript pages use the same cache. The live reducer does not retain conversation or command bodies. It owns an activity flag and one bounded foreground-shell preview; command cards format their own bounded semantic source previews. Historical formatting never borrows context or cost state from a different turn. Complete structured command values redact private fields, and incomplete structured previews expose the content action without interpreting partial JSON. These limits describe the history allocation owner; they are not a process-RSS ceiling or a bound on every live-state collection.

First-party tools declare display fields beside their result producers. Each declaration is validated and shared for the process lifetime, with an immutable generation derived from its descriptor. The engine evaluates the plan against the authoritative redacted, post-hook result and stores the bounded presentation with `ToolCallFinished`. Text, badge, list and table fields accept scalar JSON values; numbers retain their exact decimal representation, and containers are never stringified into display cells. Historical surfaces therefore use their persisted descriptor without loading a live extension catalog. Full result bodies remain independently source-addressed. The live reducer releases completed argument/result bodies after preparing a copied 4 KiB display preview and retaining the canonical output selector. Opening complete output resolves a bounded view and reads one content page at a time, including when an already-open live tool completes. Active invocation streams share an 8 MiB preview payload allowance across the admitted invocation count; omitted output is marked explicitly, and full completed content remains available from its source.

## Native viewport

The transcript mounts at most 16 historical row cards from the active page. Scrolling moves that window and requests adjoining pages at its boundaries. A logical ordinal scrollbar permits distant navigation without creating a placeholder or measured-height record for every lifetime row.

Before replacing the visible window, the renderer captures the first visible item ID and its offset from the viewport. It restores that offset after OpenTUI layout, including width changes. Following latest is explicit state: newly available rows follow the tail only while that state is enabled. A removed anchor uses the engine's replacement result.

Cards keep bounded previews and use OpenTUI for text selection, terminal cells, input and damage tracking. Unmounted historical cards are destroyed. Live reasoning and tool activity have a separate tail; durable row arrival transfers matching selection and expansion identity. A live delta does not rebuild the historical projection.

Renderer handoff is governed by `AppClientState` and its private size limit. Interactions that cannot be safely captured defer replacement. The handoff stores follow-tail intent or the visible stable source item and its signed viewport offset. A replacement resolves that item through an `around` read before restoring layout, including the index-provided replacement when rewind removed the source item. Unresolved navigation defers replacement; physical window scroll offsets are not durable history positions.

## Local diagnostics

`ROTTWEILER_CLIENT_TIMINGS=1` enables fixed-size stage counters for event/reply decoding, reply validation, reduction, presentation, history admission/update/layout and queue age. The counters contain no payloads or session identifiers and are emitted on renderer teardown. Disabled call sites do not read a clock.

Measurements are wall-clock durations. Nested stages are not additive, and asynchronous syntax parsing or terminal I/O may run outside the measured call. Process CPU and end-to-end input latency remain separate measurements.

See [Terminal workspace](terminal-workspace.md) for interaction and visual ownership.

## Client command admission

The HTTP transport owns one FIFO for ordinary direct reads, including history, catalogs, workspace inspection, and service views. `MAX_CLIENT_READS` is generated from the Rust protocol owner and limits active reads through complete reply-body consumption and validation. Conditional read watches use a separate source-classified, single-request lane so an idle watch leaves both ordinary read slots available. Actor controls and mutations bypass this FIFO and have independent source-owned ordinary and urgent count limits.

Active and waiting requests share a 32-entry, 1 MiB retained-request allowance. Admission measures a bounded JSON traversal before cloning the request; the charge covers the immutable snapshot and worst-case JSON escaping and encoding. Caller mutation cannot change an admitted request. Queue overflow rejects the request without dispatch. Aborting a waiting request removes it immediately; an active request retains admission until its HTTP operation settles. Runtime stop and session changes abort their owned reads.

The application, runtime and HTTP transport share one allocation owner created before any request. Immutable request capture, authenticated envelope construction and JSON encoding reserve outbound credit before copying or fetching. The request lease remains held through HTTP response consumption, including cancellation settlement. The caller owns decoded mutation outcomes through its final asynchronous continuation. Aggregate normal credit leaves protected capacity for the admitted urgent request and reply window; normal memory pressure cannot consume that capacity. Admission refusal preserves the previous draft and mounted projection.

Opt-in local diagnostics record `read_queue_age` in fixed counters and histogram buckets. Measurements contain no command names, session IDs, or payloads.

## Timeline actions

The conversation timeline reads semantic pages through the same charged cache as the transcript. Each page exposes older/newer navigation. Selecting a committed user source retains its exact view prefix and source identity; a source can be browsed even when the actor cannot currently rewind it.

Edit and retry read the complete first message-text block through its typed content selector. The draft owner reserves chunk, join and editable-text capacity before continuation reads. Capacity refusal or a changed content source stops the operation before a history mutation is dispatched. Original attachments are explicitly excluded from these text actions. A failed retry or edit completion merges the retained source text with any newly typed draft instead of overwriting it.

A source rewind includes the expected committed prefix, committed user source sequence, turn identity and a before/through boundary position. The actor checks these inputs inside serialized mutation admission. Reused turn numbers and stale client pages cannot select another effective source. The client applies edit/retry follow-up only after the corresponding durable rewind event carries the exact request identity. A before action without an earlier completed workspace boundary is unavailable.

Timeline handoff restores the selected source through an around read. Unresolved reads and navigation controls without a selected source defer renderer replacement. Independent history readers have distinct cache key namespaces; disposing one releases only its pages and leases.

## Task state

Task state has one authoritative typed snapshot, committed independently from transformed tool presentation. The client retains no historical task-output checkpoints. A rewind newer than the task snapshot invalidates it and records its physical sequence as the minimum acceptable read prefix. A late query cannot replace a newer live state commit.

`GetTodos` uses the authenticated read channel in live and historical views. Each query advances the mode-independent index by a bounded number of transactions and returns either an exact snapshot or explicit catch-up progress. The main view and the one presented child each own a task controller with one pending read and one catch-up timer. Opening a child loads its exact snapshot without reconstructing task state from tool output. The sidebar displays the presented session's tasks. Leaving the child, switching sessions or destroying the renderer aborts that controller; late results cannot replace another view. An authenticated reconnect refreshes the presented child's task state. Task identities, item count, per-field UTF-8 bytes and aggregate text bytes are validated from the same source schema in Rust and generated client validators.

## Asynchronous input

The draft owner shares aggregate allocation admission with history and source snapshots. It retains at most 32 MiB of editable/submitted data; a replacement reserves the old and incoming drafts together before transferring its lease. It admits one outstanding image, external-editor or history-text read. Its reservation covers the eventual retained draft and survives destination cancellation until the read settles. Submission and renderer replacement wait for accepted input reads. Image completion adds its attachment without moving the current cursor or focus; external-editor completion merges a newer draft instead of overwriting it.

Ordinary text paste is a synchronous editor operation at the initiating selection. The platform classifies explicit local image paths into deferred read capabilities before I/O; a recognized image path attaches an image or reports a read failure. Image files are descriptor-checked and bounded to 5 MiB. External-editor input/output is bounded to 2 MiB, and output is read from a checked regular-file descriptor.

## Tool surfaces

A completed tool row stores a compact title and an invocation-bound canonical source reference. Opening that reference uses the existing content read capability and exact journal view. The full declarative surface is collected from bounded UTF-8 pages, validated from its generated schema, and prepared once into native text, badge, list and table fields. Neither row paging nor frame rendering executes plugin code or repeats selector traversal.

The document owner presents one full surface at a time. It reserves collection and decode credit in the shared history cache before reading, then transfers that credit to the validated surface and prepared field strings. Cancellation retains the reservation until the outstanding read settles. Render nodes borrow the pinned model's strings and clear their content before destruction; replacement releases the previous cache lease after its nodes stop using it. Theme changes rebuild nodes against the same pinned source without another read.


## Extension panels and actions

The command palette exposes approved extension panels. One visible-session controller reads the bounded catalog and latest panel revisions through the same authenticated `SessionReader` and shared cache as transcript content. It reserves encoded, decoded and prepared allocation before reads. Polling occurs only while a panel or actionable tool surface is visible; an unchanged panel revision reuses its prepared model and native nodes. Closing, disconnecting or switching session retires picker and surface references before releasing their leases. Refresh failures keep actions disabled and allow a fresh read when the view reopens.

Native actions are declared labels and identifiers. Tab moves between content and actions; arrow keys select an action and Enter submits it. Short terminals scroll the action list within the available rows. Theme reconstruction preserves the source, content scroll and selected action focus. Rendering and keyboard handling never call extension RPC directly.

A single action owner pins the exact canonical tool source or panel revision until its ordinary engine command settles. The command includes the host-stamped extension generation, contribution and action identities, and source invocation or panel revision. It carries no client-selected command arguments. Catalog removal disables actions, and the engine validates the live generation and source before executing. Closing the view does not release an unsettled action's source; renderer replacement defers until settlement.


## Descendant read authority

A history, content or task read carries a `SessionReadTarget`: its target session and an explicit scope. A direct scope authorizes that session. A descendant scope names an independently authorized root and a bounded ancestry path. Every hop contains the child session, subagent identity and canonical spawn sequence; its parent is the root or previous hop. Cycles, excess depth and mismatched target identities are rejected.

The runtime resolves each hop through the effective transcript's indexed subagent binding, checking its immutable spawn source and child identity. Rewind removal or source reassignment revokes that association. The historical engine admits only paths rooted at its bound session; it never grants arbitrary session reads merely because the files exist. Ancestry and target projection share one four-transaction catch-up allowance per read.

The client constructs ancestry from the selected semantic child row and carries it unchanged through history paging, content continuations and task recovery. Cached document keys include authority as well as source identity. Replacing a session's read scope retires its old page windows. A selected descendant is restored through its exact bounded path and fresh control authority; leaving it returns to the root's direct authority.


Reply collection and JSON decoding use the same cache allocation owner as retained pages. Each read reserves an entry before dispatch. The transport incrementally counts strings, keys and containers without materializing them, admits buffer growth and the decoded graph, then validates the generated protocol under that charge. The consumer transfers the reservation to its exact page revision. Rejected or aborted reads retain their credit until the response reader settles; mounted values remain pinned while competing reads either evict idle entries or fail admission. Content inspection, timeline source restoration and child history use this contract as well. Optional `reply_allocation` timing counters measure the structure-counting work without recording payloads.


The question projection owns unresolved interactions only. A durable answer releases the corresponding question body; the canonical journal retains the exchange. Question request count and serialized payload admission use generated producer limits. Citation count and UTF-8 bytes are accumulated for the active agent turn using the same source-owned ceilings. An inadmissible event is rejected before advancing the durable client cursor; unresolved interactions are never evicted to make room.


Approval payloads remain exact while a decision is unresolved. Completion releases rationale and capability lists, retaining at most a copied 16 KiB inline diff plus its canonical diff selector. Larger proposals and late diff updates keep the source action without retaining the complete proposal in the live tool map. Complete diff inspection resolves an authenticated bounded content view; historical rows continue to use their pinned view.

## Live session bootstrap

A fresh renderer, authenticated reconnect or rejected replay cursor collects source-backed live display components, session metadata, unresolved controls, active children and task state before subscribing. Each component retains its exact applied source prefix. The subscription starts at the minimum of those prefixes; reducer fences suppress only events already covered by the corresponding component. The supervisor cursor alone never authorizes skipping state. Historical inspection uses its session-bound read capability without opening an actor or taking a driver lease.

The display tail reads text, thinking, citations and invocation previews as bounded pages from one structural epoch. Text and thinking each expose at most 64 KiB and report omitted content explicitly. Complete bodies remain available through canonical content selectors. A changed active-turn source or projection epoch causes the collector to release the incomplete bundle and retry. Compaction preview uses the actor's independent attempt/revision fence, including missed transient updates.

Session metadata includes source-qualified plugin status values. Controls preserve exact pending question, approval and plan payloads; the active child snapshot includes bounded canonical spawn identities and task previews. These snapshots do not infer authority from tool presentation or a later query's cursor.

The client allocation owner charges incoming snapshot decoding alongside the mounted source bundle. Only one bootstrap collector runs at a time, including cancellation settlement. Installation retains the old and incoming owners while component references change. A failed partial rebind retains both owners until disposal and rejects further installation. Component destruction severs retained model references before the application releases its snapshot bundle. Drafts and active input ownership survive a successful renderer handoff independently of source state.

Normal presentation defers only display deltas and progress. That slot retains the newest reduced state, without the raw delta or previous projection; subsequent display updates replace it. Query replies, control transitions and commands flush the display slot and present their effects synchronously. Historical replay similarly retains one final projection. A stalled frame therefore cannot retain a chain of intermediate tool maps or text revisions.

Live tool cards retain immutable bounded chunks and incrementally computed line windows. Their shared preview cache contains no complete plain or labeled output strings. The open output viewer owns those strings through a dedicated incremental reader, and releases them when the viewer closes, replaces its source, or receives final output. Canonical output is available through source-qualified content pages.

The aggregate client allocation ceiling is 256 MiB of conservative payload credit. Domain ceilings share that total; they do not add independent capacity. Root and active-child models, mounted revisions, history and document readers, drafts, snapshot collection, and decoder reservations compete for this owner. Immutable projections share object-graph credit across revisions. Tool streams use immutable cumulative source costs and transaction-owned retained heads rather than one accounting record per chunk; their bounded preview cache holds a weak source cursor and cannot keep newer source nodes alive through an older prefix. A synchronous event effect retains its old model until the callback returns. Native rebind failure retains both referenced revisions, and allocation refusal preserves the previous state. Native renderer and allocator overhead are measured separately from these payload credits.


## Family control discovery

The live parent connection owns one conditional family-control read. Its snapshot contains bounded child identities and pending counts, independently of lossy progress updates. Reconnect starts with an unconditional read because the revision belongs to the live host. The child picker and parent banner expose pending questions, approvals and plan reviews without opening every child or retaining a union of their control bodies.

Only the selected child receives a full control snapshot. Its exact live ancestry, actor revision and question or invocation identity travel with every explicit response. A newer discovery revision disables action admission until the selected snapshot is refreshed. Switching children releases the displayed snapshot and preserves any in-flight response owner until settlement. Child control responses never grant parent permissions. Live control authority is separate from history: an exact binding query resolves every selected ancestry hop against its canonical spawn source before transcript, task or content reads. The query includes retained terminal children by indexed identity; it does not enumerate lifetime history or rely on the active-child list. Controls remain usable while the display index catches up.

The child catalog holds its own allocation lease independently of decoded replies and mounted picker revisions. Task, model and agent previews are byte-limited before grapheme formatting. Replacement reserves preparation alongside the retained catalog; admission failure preserves the previous bindings and charge.

The generic picker owns its complete item values, labels and filtering/render preparation through the shared allocation ledger. Replacement admits the incoming revision beside the mounted one before changing native nodes. Selection keeps that exact revision charged through its callback even when the callback closes the picker. A partially failed native replacement retains both revisions until teardown; stale callbacks cannot act on another visible list. Recursive destruction clears picker data before destroying its child renderables.

The selected live child has one metadata/display worker, independent of the family-control watch. It polls scalar state at most four times per second and collects canonical text, thinking, citations and invocation previews only when the durable prefix changes. Compaction preview has its own revision and can update without rereading the durable tail. Each tail must cover the metadata prefix and match the actor's actual active-turn source. Progress messages invalidate source views without acting as the selected child's control or display authority. Reconnect revalidates the exact live binding before reopening display reads. Leaving the child cancels polling; decoder, source and returned-page credit remain charged until outstanding work settles.


## Compiled ownership measurement

`python3 packages/tui/scripts/client-memory-probe.py --candidate PATH --output PATH` verifies an existing native candidate and runs its compiled TUI through three distinct processes, with twenty workload cycles per process. It does not build during measurement. The workload mounts a 10,000-row paged history, a 128-entry child picker, a 64 KiB editable draft with an attachment, sixteen active tool previews, pending questions, and source-paged output. Two real Unix HTTP reads overlap a mutation; decoded read and mutation replies remain held through asynchronous consumer settlement. Refused projections, cancelled reads, malformed replies and closed-viewer responses must preserve or retire their exact owners.

Reports retain workload parameters, allocation-domain observations, current resident memory, native high-water resident memory, and zero-allocation teardown checks. The admission-refusal step reserves credit without allocating a corresponding payload; this is reported separately from actual resident memory. The bounded protocol fixture runs inside the measured process. Explicit exit-75 capture and restoration exercise handoff independently of the production RSS threshold. The engine-plus-TUI RSS and soak gates measure the complete application envelope.

Renderer handoff retains the local composer, highlighted control option, and selected child identity in a private bounded file. A child selection is matched against fresh family discovery, then its canonical read scope and current scalar/control state are read again. A historical child remains a source-qualified passive view. No saved revision or projection grants action authority. Control highlights match a fingerprint of the complete displayed control; changed prompts, invocations or plans do not inherit a selection. Unfinished free-text answers remain editable, and an unchanged answer cannot be sent to a different question. Editing the answer releases that local binding.

Interactive composer text is limited to 128 KiB UTF-8 before mutation; larger content uses file attachments. Set, restore, native insertion, selected replacement, external-editor return and private handoff use the same limit. Pending submissions reserve their rollback text capacity. Native undo shares unchanged document roots and charges edit deltas rather than a full-document copy per keystroke.

Handoff admits at most 8 MiB encoded and 64 MiB prepared, using the same JSON shape calculation before writing and before decoding. Editable drafts must also fit the aggregate draft owner. Handoff serialization holds shared preparation credit. Private-file decoding opens a single descriptor without following symlinks, bounds bytes before reading, and holds decode credit through application adoption. The private file is consumed only after editable state has been admitted; reconnect queries begin after this transfer, preserving allocation headroom. Pending view/selection restoration has its own retained allocation until completion, cancellation or teardown. In-flight submissions, external input operations, secret-entry forms and source navigation still require settlement before handoff.
