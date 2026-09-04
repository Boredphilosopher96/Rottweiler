# Mission architecture remediation

Source assessment: [architecture review](2026-09-04-mission-architecture-review.md).
Authorized September 4, 2026: implement all findings, without backward compatibility constraints.

## Execution phases

- [x] Ground: source review, live CI evidence, project contracts.
- [x] Sketch: CI ownership designs compared; runtime contracts are designed per dependency-ordered work unit.
- [x] Agree: synthesize designs and record decisions. User authorized implementation without an additional checkpoint.
- [ ] Implement: CI stabilization, runtime liveness, journal/client state, scheduling, extensibility, workspace/context, qualification.
- [ ] Reassess: replace designs that fail integration or measurement; no silent scope reduction.

## Status rules

Pending means no completion claim. Source complete needs checked-in implementation and focused verification. Verified needs integrated checks. Hosted qualification requires the exact candidate to execute on the required platforms. Infrastructure unavailable is not a passing result.

## Findings

| ID | Work | Status | Evidence / remaining work |
| --- | --- | --- | --- |
| C01 | P1: Restore a trustworthy main branch and complete gate ownership | Source implemented; hosted pending | Always-run required aggregate, semantic workflow validation, independent package/fuzz checks. Hosted ruleset update waits for passing candidate. |
| C02 | P1: Contract changes must compile and test every consumer | Source implemented; integration pending | Package inventory, native Bun updater coverage, compile every fuzz bin, corrected decoder owner and visual golden. Stable fuzz compile passed. |
| C03 | P1: Fix the platform boundary exposed by scaffold generation | Focused verification passed | Pinned Bun SDK suite, packaged scaffold LF/CRLF and refusal tests; real replay CLI golden passed. WSL hosted qualification pending. |
| C04 | P1: Protected jobs need real runner capacity and a bounded queue | Nightly queue implementation verified locally; capacity unavailable | Separate platform workers, actual capacity guard, fifteen-minute hosted watcher, bootstrap/upload deadlines, exact owned cancellation and attempt-qualified bundles. Eleven dispatcher/queue tests pass. Zero registered runners; hosted qualification and v1 release integration remain pending. |
| C05 | P1: Terminal acceptance and soak need explicit readiness and progress evidence | Short macOS lifecycle verified; qualification pending | b064827: model discovery and composer readiness, platform-correct RSS, periodic soak diagnostics. Fresh combined TUI passed lifecycle and 30.788s PTY soak: 66/66 completed, 17 tool turns, 16 compactions, one forced restart, 338067456-byte peak. Eight-hour/native Linux unrun. |
| C06 | P1: Make performance and artifact failures attributable without weakening budgets | Focused build and performance verification passed | b064827: Linux duplicate native payload removed (151570992 to 113674800 bytes), all five grammar smoke checks passed under emulation; pinned local performance suite passed with raw samples retained. Native hosted qualification and build reuse topology remain pending. |
| C07 | P1: Dependency maintenance must reconcile configuration and lockfiles | Source implemented; hosted pending | Wasmtime47.0.4 advisory gate passes; native Bun updater and plugin-host/tooling coverage added. Hosted updater reconciliation awaits merged config. |
| C08 | P1: Every failed gate must leave usable evidence | Source diagnostics implemented; hosted failure qualification pending | Bounded gate results, actual checkout SHA, bundle digests, atomic soak progress and minute metadata log checkpoints. Local wrapper/cancellation tests pass. Hosted artifact retention and abrupt runner-loss behavior remain to exercise. |
| A01 | P1: Ordered output has a circular wait under saturation | Integrated verification passed | Full workspace all-feature tests, all-target clippy and format passed. Full-engine saturation and cancellation cleanup regressions pass; bounds and deterministic output order preserved. |
| A02 | P1: Reconnect and history paging repeatedly process the lifetime journal | Pending | |
| A03 | P1: Storage latency still controls actor responsiveness | Pending | |
| A04 | P1: The transcript is a recent tail, not a virtualized history | Pending | |
| A05 | P1: Renderer recycling is normal memory control and loses client state | Recovery state checkpoint verified; memory qualification pending | b064827: app/component-owned client state, private bounded handoff, attachment validation, input selection and folds restored. Unsafe active interactions defer recycling. 536 TUI tests and fresh lifecycle/short soak passed; native memory ownership and long-soak deferral behavior remain to qualify. |
| A06 | P1: Plugin SDK request handling can block its own replies | SDK checkpoint verified; host stream isolation pending | e57572f: independent bounded input pump, actual handler admission, bounded outbound/HTTP queues and shutdown. 61 SDK tests, typecheck and build passed. Host provider stream credits/isolation still need the extension runtime work. |
| A07 | P1: Extension control is too narrow for the stated mission | Pending | |
| A08 | P2: The runtime boundary still owns terminal presentation | Pending | |
| A09 | P2: Command admission and retry semantics need stronger ownership | Pending | |
| A10 | P2: Tool scheduling is all-parallel or all-serial | Pending | |
| A11 | P2: Context assembly repeats work on unchanged content | Pending | |
| A12 | P2: Spend limits observe usage but do not reserve in-flight cost | Pending | |
| A13 | P2: Checkpoint capture can allocate without a file-size budget | Pending | |
| A14 | P2: Workspace intelligence lacks an aggregate cache and freshness contract | Pending | |
| A15 | P2: The client accepts typed-looking wire data without validating its shape | Checkpoint verified; global integration pending | b064827: Rust-schema-generated standalone validators reject malformed known events before reducer/cursor changes. Full 536-test TUI suite, typecheck and codegen passed; compiled cost included in C06. |
| A16 | P2: Client memory bounds need bytes and allocation accounting | Implementation in progress | Bounded SSE assembly, tool-output projections and display/size allocation work underway. Aggregate history/artifact cache ownership depends on A02/A04. |
| A17 | P2: UI interaction policy is centralized without a single state model | Pending | |
| A18 | P2: Rich presentation has no third-party contribution contract | Pending | |
| A19 | P1: Current performance evidence does not prove the mission | Partial; baseline qualification pending | True10MiB fixture and retained raw samples; measured local ceilings pass. Controlled platform baselines need recollection. |
| A20 | P2: Performance diagnosis lacks stage-level ownership | Pending | |
| A21 | P1: Cancelling an ordinary plugin request does not settle its effects | Integrated ordinary-request settlement; containment gap remains | 115 extension tests and full workspace checks pass. Host pushes drain to actor outcomes, owned HTTP closes, and plugin process-group settlement precedes checkpoints. Deliberately escaped native macOS descendants remain a containment gap; builtin Bash drop/lease audit underway. |
| A22 | P2: WASM hooks pay process and compilation costs per invocation | Pending | |
| A23 | P2: Plugin tools lack a proper long-operation contract | Pending | |
| A24 | P2: Plugin pushes discard the host's outcome | Pending | |
| A25 | P2: Plugin effect precision stops at process authority | Pending | |
| A26 | P2: Extension event delivery can lose state without recovery | Pending | |
| A27 | P2: Hook semantics and latency budgets need stronger contracts | Pending | |
| A28 | P2: Enabled plugins impose eager serial startup work | Pending | |
| A29 | P2: MCP needs a negotiated inbound-capability owner | Pending | |
| A30 | P2: Provider evolution needs typed content and continuation seams | Pending | |
| A31 | P2: Durable work needs an identity beyond a turn or tool call | Pending | |
