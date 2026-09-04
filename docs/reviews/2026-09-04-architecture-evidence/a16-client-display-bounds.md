# A16 client display and allocation checkpoint

This is a bounded part of A16, implemented on `feat/mission-client-bounds` from `035ecdc`. It does not close aggregate session, artifact, transcript, or child-cache memory accounting. Tests use pinned Bun 1.3.14.

## Shape and ownership

The reducer owns immutable display projections. Tool output now appends one bounded chain node instead of copying the previous chunk array. A mutable shared array would be simpler but would change older reducer snapshots. Rebuilding joined output on every read would preserve snapshots but repeat prefix traversal and copying. The chosen chain has a weakly keyed, single-current-version materialization cache. Forward reads visit new nodes; older snapshots and branches rebuild from their own immutable nodes without corrupting newer results. The cache value has no reference to its weak key. No global strong collection retains abandoned streams. A retained old snapshot can keep that stream's one newer cached materialization alive, still within the per-stream cap; the implementation does not cache every historical materialization.

Each tool retains at most 1 MiB + 1 KiB and 1,025 chunks. The allowance accommodates the engine's existing 1 MiB/1,024-chunk limit plus its terminal truncation marker. Further payload is counted as omitted and visibly marked. A final tool event replaces the live buffer with the empty singleton and uses the authoritative final output. Earlier reducer snapshots remain readable. Tool projection buffers are not protocol or `AppClientState` handoff payloads; their JSON diagnostics contain counts, not recursive history.

The materialization maintains raw transcript and normalized Tools line windows, each holding at most 32 lines. Each preview line keeps at most 4,096 UTF-16 code units and displays `[earlier characters omitted]` when it drops a prefix. Truncated suffixes are copied so they cannot retain a large temporary backing string. Mounted live cards and Tools rows consume these windows rather than splitting the complete accumulated output every frame. The full-output viewer still reads the complete retained per-tool body. Preview clipping does not incorrectly mark that retained body as lost.

Active text and reasoning each retain a 1 MiB UTF-8 prefix with exact retained/omitted-byte metadata and an omission marker. Once a prefix is truncated, later deltas cannot fill the remaining byte gap and silently produce text with missing content in its middle. UTF-8 clipping avoids splitting surrogate pairs. These are client display budgets, not edits to durable engine conversation content.

SSE partial lines now use geometrically grown storage bounded by the existing line limit. The parser yields events incrementally rather than first allocating an array for every event in an incoming chunk. It also caps data fields per event at 1,024. A completed large partial line releases its scratch buffer; small scratch buffers can be reused. Existing event-byte limits and transport cursor/error semantics remain.

Composer, attachment, todo and child-event byte counts use `Buffer.byteLength` instead of allocating an encoded buffer only to obtain its length. Exact JSON wire-size checks still serialize JSON once. They do not claim zero-copy attachment handling.

## Reproducible evidence

`packages/tui/test/display-buffer.test.ts` exercises production transcript/Tools readers, interleaved old/new/branched snapshots, unchanged reads, final replacement, host marker fragmentation, byte/chunk caps, empty-chunk exhaustion, Unicode boundaries and independent text/reasoning budgets. A 1,000-chunk reader test observes the real `String.split` and `TextEncoder.encode` calls. Splits process less than four times admitted payload, unchanged reads do not rescan output, and the normal reader path makes no `TextEncoder` allocation. A separate 1,000-chunk single-line case proves preview copy sizes stay bounded while all 1,000,000 retained characters remain accessible.

For 1,000 chunks totaling 1,016,000 bytes, the materialization visits 1,000 nodes and feeds 3,054,999 code units into its three window builders. Only one current materialization is cached. These counters measure explicit traversal/input work, not JavaScriptCore heap allocation or native renderer allocations.

`packages/tui/test/sse.test.ts` feeds a 131,080-byte wire event one byte at a time. Measured scratch storage work is 392,198 copied bytes and 523,264 allocated bytes, with zero retained scratch bytes after completion. The assertions require each cumulative figure below four times wire size. Existing legal 5 MiB image transport coverage remains. An additional test proves the first event is yielded before a later oversized event fails.

Validation commands, run from `packages/tui` with pinned Bun on `PATH`:

- `bun run typecheck`
- `bun run test`
- `bun run test:perf`
- `bun run build`
- `git diff --check`

Validation passed: **544 normal TUI tests, 21 snapshots and 12,154 assertions**; all **6 performance tests**; typecheck; release build; and diff checks. The local transport p99 was 1.142 ms, with focused/Vim input best-trial p99s of 1.859/1.297 ms. These are shared-host observations, not calibrated release claims. The release bundle was **82,899,120 bytes**, below the unchanged 100,000,000-byte budget. Logs are `/tmp/rw-a16-all-tui.log`, `/tmp/rw-a16-perf.log` and `/tmp/rw-a16-build.log`.

Normal TUI tests include the real native visual harness and unchanged golden snapshots. The harness fixture constructors were migrated after a full-suite run exposed that scripts are outside the ordinary typecheck include list. No expected visual output was regenerated.

## Limits of this checkpoint

Logical UTF-8 budgets are not an RSS ceiling. JavaScript strings, metadata, cached labeled output, renderer state and retained older snapshots add memory. Full-output presentation and streaming Markdown still have rendering work proportional to retained content. Durable final outputs, transcript paging, rewind state, approval diffs, image payload retention and aggregate child/session caches remain coordinated A02/A04 work. This checkpoint does not establish protected-runner latency, hosted CI, an eight-hour soak, or whole-client aggregate memory compliance.
