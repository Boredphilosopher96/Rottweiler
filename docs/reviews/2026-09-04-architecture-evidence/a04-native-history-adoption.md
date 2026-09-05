# Native history adoption checkpoint

Status: production native-page adoption, verified locally on macOS arm64 with
Bun 1.3.14 and OpenTUI 0.5.10. This checkpoint does not close the remaining A09
host-control ownership, A16 legacy live-state retention, or A20 timing work.

## Production change

The TUI composition root and actual `rw replay` worker supply the runtime's
required `HistoryReader` capability. The historical CLI host captures bounded
bootstrap metadata and serves semantic pages/content through `HostReadChannel`;
it no longer collects a lifetime event vector before launching the renderer.
Its availability event does not advance the durable event cursor or report raw
replay completion. Raw JSONL export remains a separate canonical-history path.

The viewport consumes `TranscriptItem` directly. Sixteen retained rows form the
physical window over a byte/count-bounded 32-item page. The logical scrollbar
uses semantic ordinals while source IDs and pixel offsets preserve the visible
anchor through replacement, rewrap, child-session visits, and cache eviction.
OpenTUI's post-layout hook applies anchors after descendant measurement. Applying
a page does not rerun the complete application presentation update.

One 16 MiB/2,048-entry LRU owns semantic pages and byte-paged document chunks for
up to eight session views. Explicit leases keep a mounted old revision charged
until its replacement has been presented. Failed admission is atomic; obsolete
responses cannot republish an old session or source prefix. Content reads are
4 KiB UTF-8 chunks with a bounded 16 MiB document and offset index. Parent and
child pages share that owner rather than receiving independent per-child caps.

Host invocation IDs bind live tools to final semantic tool rows. Final output
has one visible row; expansion/keyboard selection survives handoff. Reasoning
uses a separate keyboard block and keeps its collapsed state through commit.
Turn summaries are sourced only from TurnFinished. Projection version 3 also
retains child touched-file counts and a canonical SubagentDiff content selector;
child navigation and opening its patch are separate actions. The old wire
ReplaySubagent command/events, runtime raw replay helpers, and client raw child
reducer/cache are deleted in the same wave.

## Verification

- Complete TUI run: **557 passed, 0 failed; 20 snapshots; 52,541 assertions**.
  Includes all five unchanged performance gates, the persistent-SSE p99 <2 ms
  gate, actual supervised approval, native scroll/selection/clipboard tests,
  document eviction/reload and fragmented authenticated reads.
- Actual Rust `rw replay` acceptance: **1 passed**. Nine persisted canonical
  events become four semantic rows. Availability reports prefix 8; both
  lastSequence and completedThrough remain null because this fixture emits no
  durable bootstrap event. Semantic assertions are separate from the reviewed
  terminal golden. Three historical-host unit tests also pass.
- Core transcript tests: **16 passed, 1 diagnostic benchmark ignored**. Includes
  canonical child-patch chunk reconstruction, signature-only reasoning filtering,
  source identity, rewind and bounded projector restart coverage.
- Strict all-target clippy for types/core/runtime/CLI/xtask, codegen --check,
  TypeScript typecheck, and frozen Bun install pass.
- Compiled TUI: **85,598,800 bytes**, below the unchanged 100,000,000-byte budget.
- The real Tree-sitter visual harness passes its original gutter, role-color,
  indentation and right-rail assertions. The native conversation PNG was inspected.
  Tool fixture identities are now required and the visual harness is typechecked.

The 162 application tests and 67 component tests now live in feature-owned test
modules. Transcript rendering is split into viewport, native row, live blocks
and post-layout scroller owners. The visual harness separates scenarios from
rendering/evidence assertions. Each of those extracted handwritten files is
below 1,500 lines.

## Limits and next work

This aggregate owner covers the adopted semantic-history/document path. The
parent reducer still retains legacy conversation/turn/tool projections for
other callers, and the old live tool-output viewer is not yet charged to this
cache. Their removal/admission is the next A16 unit; these tests are not a claim
that total process RSS is now bounded for arbitrary sessions. Cache teardown and
physical process-recycle handoff also need the final logical-anchor ownership
contract before that broader claim.

The historical fixture has no SessionCreated or durable metadata record, so its
model label is unavailable in bounded bootstrap. The new footer displays exact
TurnFinished usage/cost independently. A02's bounded recovery bootstrap owns
remaining cold live state; an unfinished provider stream is not reconstructed
by displaying semantic history alone.

A09 must still replace the host mutation completion/dedupe owner and give
shutdown a barrier over accepted control work. A20 needs production opt-in client
stage timings and overhead evidence. The large app/reducer/panel/test owners
still require their remaining semantic extractions under the 1,500-line cap.
