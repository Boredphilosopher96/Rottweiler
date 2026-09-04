# A05, A06 and A15 implementation evidence

This records the local implementation and validation checkpoint. It does not replace the original reproductions or claim hosted CI, a full long soak, or the complete architectural migration is finished. Tests used the repository-pinned Bun 1.3.14.

## A05 — preserve client state at renderer replacement

The app now owns one `AppClientState` contract, validated by the existing private recycle-handoff owner. It carries composer text and attachments, cursor and selection, hidden child drafts, the primary route and scroll positions, input mode and focus, theme, safe picker query/selection/viewport, and component-owned transcript/Tools selection and folding state. Theme rebuilds use the same editing and block-state capture/restore methods.

A snapshot was chosen over moving all client state outside the renderer because supervision replaces the entire Bun process. Moving state to another object in that process alone cannot preserve it. The existing one-shot private file remains the process handoff, distinct from durable conversation history. The 8 MiB total cap, private permissions and atomic replacement remain. The new schema rejects old or malformed state rather than silently omitting fields.

Sensitive or callback-dependent interactions defer replacement: credential entry, MCP configuration input, pending authentication/submission/export/rewind, shell handover, active child inspection, approval/question panels, review and full-output overlays. A handoff exceeding the cap or a failed write also defers replacement. No credential-entry field is serialized. Hidden child drafts are limited to 256 entries and each expansion list to 4,096 entries. Over-limit state is refused as a whole. Deferred attempts are retried at most once per ten seconds while above the unchanged 384 MiB threshold.

`recycleTuiIfNeeded` is the production threshold/capture/write/exit decision, called by `index.ts`. Its test forces 500 MiB and proves an active review stays alive, a missing handoff destination cannot trigger exit, and a valid private handoff permits replacement. Native OpenTUI tests capture, serialize, destroy, recreate and restore the app, proving attachment contents, editing selection, active palette selection/focus, Tools route and transcript/Tools folds survive. This preserves interactions; it does not guarantee the combined RSS ceiling while replacement is deferred.

Files: `packages/tui/src/app.ts`, `src/recycle-state.ts`, the recycle callsite in `src/index.ts`, component state methods in `src/components/{transcript,tools-workspace}.ts`, and `test/client-state.test.ts` plus migrated handoff tests. Other contemporaneous changes in those files belong to the RSS/onboarding and C06 work.

## A06 — SDK duplex progress and bounded outbound work

The server now admits bounded handler tasks while the input loop continues dispatching HTTP responses/events and cancellation. Merely handling control replies inline would leave the original deadlock intact when an inline catalog handler awaits HTTP. Admission is limited to 64 handlers, and actual callbacks remain charged if they ignore a timeout or cancellation. This avoids gaining unlimited admission by repeatedly timing out uncooperative callbacks.

The outbound writer uses a FIFO with limits on aggregate encoded bytes and frame count, including the in-flight write: 16 MiB and 256 frames. Overflow, sink failure or a write deadline ends the transport and rejects pending writes; it does not silently acknowledge dropped responses. The server write deadline uses its handler timeout (5 seconds by default). External cancellation and shutdown settle pending calls, including a provider iterator that ignores abort. JavaScript cannot forcibly stop arbitrary callback code; the SDK retains admission accounting until such code settles.

Host-mediated HTTP remains provider-scoped and correlated. The SDK admits at most 64 HTTP requests, with a 64-chunk/4 MiB body buffer per request. Buffer overflow cancels that HTTP request explicitly; it no longer pauses the sole input reader and prevents later control messages from progressing. Abort listeners are removed after correlated completion.

The new production `serve` regression was run against an isolated copy of pre-change HEAD: catalog response id 2 failed with `-32004: plugin handler timed out`. It passes with the new implementation. Historical probes remain unchanged. New tests also cover saturation with HTTP progress, shutdown, uncooperative provider cancellation, secret-safe permission errors, provider scoping, FIFO, aggregate-byte/frame limits, failed writes and stuck shutdown output.

Validation: full SDK suite **61 passed, 221 assertions**; typecheck and package build passed. Baseline failure log: `/tmp/rw-a06-before.log`. Files: `packages/plugin-sdk/src/{server,transport}.ts`, `test/duplex.test.ts`.

## A15 — validate known engine events once at transport ingress

A generated standalone AJV validator is compiled from the Rust-owned engine-event JSON Schema. This was chosen over runtime schema compilation: Rust remains the source of truth, `cargo xtask codegen --check` checks the projection, and deployed TUI code requires neither AJV nor runtime compilation. AJV 8.20.0 is pinned as a development dependency. Generation enforces the pinned Bun version.

Known discriminators receive full generated payload validation at the transport boundary. Existing protocol-version and durable u64 identifier semantics remain enforced by a shared parser in the protocol-facing transport type owner. Transient/connection events cannot advance the durable cursor through additive metadata. Unknown event handling retains the existing compatibility policy. No payload validation was added to rendering.

Malformed known payloads or invalid JSON produce a terminal `EngineProtocolError`; invalid frames are not delivered to the reducer and do not advance the cursor. Tests cover all Rust fixture events, removal of required fields, malformed nested payloads, protocol versions, full u64 identifiers, additive/unknown events, and actual authenticated UDS transport cancellation/cursor behavior.

Validation: `cargo run --locked --quiet -p xtask -- codegen --check`, pinned install, validator check and typecheck passed. The earlier A15 full suite passed 519 tests; the combined A05/A15/RSS/onboarding checkpoint passes **536 tests, 21 snapshots, 7,664 assertions**. Full TUI log: `/tmp/rw-a05-all-tui.log`. Global `git diff --check` passed at the checkpoint. This is local evidence, not final hosted CI or release-bundle qualification.
