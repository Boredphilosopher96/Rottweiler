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
| C01 | P1: Restore a trustworthy main branch and complete gate ownership | Candidate verified and merged; post-merge CI running | PR54 candidate6c3a4ae passed every gate in run33930648431. Main3ef57e9 has the identical tree. Ruleset19590466 now requires CI required from GitHub Actions; every unrelated rule is unchanged. Post-merge run33931348188 pending. |
| C02 | P1: Contract changes must compile and test every consumer | Integrated and hosted verification passed | Package inventory,native Bun updater coverage,every fuzz bin and generated decoder ownership. All eight package jobs,fuzz compilation and both OS integration jobs passed at6c3a4ae. |
| C03 | P1: Fix platform boundaries exposed by scaffolding and native fixtures | Focused verification passed | SDK clean-pack exports and LF/CRLF scaffold tests pass. LSP teardown fixture no longer depends on the macOS Python shim; protocol fixture resolves the actual interpreter outside its sandbox. Seven intelligence tests pass. WSL qualification remains pending. |
| C04 | P1: Protected jobs need real runner capacity and a bounded queue | Nightly and release queue implementation verified locally; capacity unavailable | Separate platform workers, actual capacity guard, fifteen-minute hosted watcher, bootstrap/upload deadlines, exact owned cancellation and attempt-qualified bundles. Fourteen dispatcher/queue tests pass. Exact-tag release queue watcher and fresh-run admission prevent unwatched failed-job reruns. Zero registered runners; hosted qualification remains pending. |
| C05 | P1: Terminal acceptance and soak need explicit readiness and progress evidence | Short macOS lifecycle verified; qualification pending | At source03d9cce: model discovery, composer readiness, platform-correct RSS and periodic soak diagnostics. Compiled lifecycle and30.793s PTY soak passed:66/66 completed,17 tool turns,16 compactions,one forced restart,346587136-byte peak. This predates subsequent Bash/shutdown/A24 changes. Eight-hour/native Linux unrun. |
| C06 | P1: Make performance and artifact failures attributable without weakening budgets | Focused build and performance verification passed | b064827: Linux duplicate native payload removed (151570992 to 113674800 bytes), all five grammar smoke checks passed under emulation; pinned local performance suite passed with raw samples retained. Native hosted qualification and build reuse topology remain pending. |
| C07 | P1: Dependency maintenance must reconcile configuration and lockfiles | Source implemented; hosted pending | Wasmtime47.0.4 advisory gate passes; native Bun updater and plugin-host/tooling coverage added. Hosted updater reconciliation awaits merged config. |
| C08 | P1: Every failed gate must leave usable evidence | Source diagnostics implemented; hosted failure qualification pending | Bounded gate results, actual checkout SHA, bundle digests, atomic soak progress and minute metadata log checkpoints. Local wrapper/cancellation tests pass. Hosted artifact retention and abrupt runner-loss behavior remain to exercise. |
| A01 | P1: Ordered output has a circular wait under saturation | Integrated verification passed | Full workspace all-feature tests, all-target clippy and format passed. Full-engine saturation and cancellation cleanup regressions pass; bounds and deterministic output order preserved. |
| A02 | P1: Reconnect and history paging repeatedly process the lifetime journal | Isolated implementation underway | Segmented journal and pinned-prefix foundation verified at 10K/100K/1M events. Production caller/recovery migration still pending integration. |
| A03 | P1: Storage latency still controls actor responsiveness | Pending | |
| A04 | P1: The transcript is a recent tail, not a virtualized history | Design recorded; implementation pending | Semantic transcript index and bounded page/cache/viewport contract under review; requires A02 production read views. |
| A05 | P1: Renderer recycling is normal memory control and loses client state | Recovery state checkpoint verified; memory qualification pending | b064827: app/component-owned client state, private bounded handoff, attachment validation, input selection and folds restored. Unsafe active interactions defer recycling. 536 TUI tests and fresh lifecycle/short soak passed; native memory ownership and long-soak deferral behavior remain to qualify. |
| A06 | P1: Plugin SDK request handling can block its own replies | SDK and host-command checkpoints verified; provider stream isolation pending | Bounded SDK input/output and host-command admission; host reader correlates replies while owned actor work runs. Integrated SDK65 tests,typecheck/build pass. Provider stream credits and long-operation isolation remain in progress. |
| A07 | P1: Extension control is too narrow for the stated mission | Pending | |
| A08 | P2: The runtime boundary still owns terminal presentation | Pending | |
| A09 | P2: Command admission and retry semantics need stronger ownership | Pending | |
| A10 | P2: Tool scheduling is all-parallel or all-serial | Per-batch scheduler checkpoint verified; shared resource admission pending | Sixteen-call ordered read windows separated by mutation barriers;completed later results remain charged. All311 core tests,strict clippy and format pass. Aggregate multi-session resource budgets and provider call-list/argument admission remain pending. |
| A11 | P2: Context assembly repeats work on unchanged content | JSON estimation allocation checkpoint verified; incremental assembly pending | Canonical-byte sizing without JSON clones/output buffers;43 context unit tests,accuracy integration,clippy and format pass. Retained diagnostic benchmark samples;no controlled p99 claim. Revision-based block/prefix caching and running totals remain pending. |
| A12 | P2: Spend limits observe usage but do not reserve in-flight cost | Pending | |
| A13 | P2: Checkpoint capture can allocate without a file-size budget | File-content memory checkpoint verified locally | 64 KiB streamed capture/fingerprinting,64 MiB preimage limit, safe temporary cleanup and file-version checks. Nineteen checkpoint tests and store clippy pass. Aggregate quotas, bounded inventory metadata/Git output and cancellable scans remain pending. |
| A14 | P2: Workspace intelligence lacks an aggregate cache and freshness contract | Pending | |
| A15 | P2: The client accepts typed-looking wire data without validating its shape | Integrated verification passed | b064827: Rust-schema-generated standalone validators reject malformed known events before reducer/cursor changes. Full 536-test TUI suite, typecheck and codegen passed; compiled cost included in C06. |
| A16 | P2: Client memory bounds need bytes and allocation accounting | Local integration and hosted TUI smoke passed; aggregate caches pending | Bounded SSE,immutable tool buffers/display projections and redundant rendering work removed.546 TUI tests,typecheck/build pass. Both hosted TUI smoke gates passed at bd631ca with unchanged budgets. Aggregate history/artifact cache ownership depends on A02/A04. |
| A17 | P2: UI interaction policy is centralized without a single state model | Pending | |
| A18 | P2: Rich presentation has no third-party contribution contract | Pending | |
| A19 | P1: Current performance evidence does not prove the mission | Partial; baseline qualification pending | True10MiB fixture and retained raw samples; measured local ceilings pass. Controlled platform baselines need recollection. |
| A20 | P2: Performance diagnosis lacks stage-level ownership | Pending | |
| A21 | P1: Cancelling an ordinary plugin request does not settle its effects | Ordinary request,Bash and shutdown checkpoints verified; containment gap remains | Owned process-group/host-effect settlement and builtin Bash drop/lease fixes integrated. Shutdown now shares settlement proof;native macOS fixture plus ten repeats pass. Deliberately escaped native macOS descendants remain a containment gap. Provider lifecycle forwarding remains underway. |
| A22 | P2: WASM hooks pay process and compilation costs per invocation | Pending | |
| A23 | P2: Plugin tools lack a proper long-operation contract | Pending | |
| A24 | P2: Plugin pushes discard the host's outcome | Correlated command checkpoint verified | Typed SDK injection disposition and host errors;64 owned host commands,duplicate ID refusal,reader independence. Actor work retains settlement permits through outcomes;panic leaves effects unproven.65 integrated SDK tests pass. Full protocol-3 lifecycle migration remains pending. Integrated workspace:1361 tests passed,4 ignored;clippy,format and both code generators passed. |
| A25 | P2: Plugin effect precision stops at process authority | Pending | |
| A26 | P2: Extension event delivery can lose state without recovery | Pending | |
| A27 | P2: Hook semantics and latency budgets need stronger contracts | Pending | |
| A28 | P2: Enabled plugins impose eager serial startup work | Pending | |
| A29 | P2: MCP needs a negotiated inbound-capability owner | Pending | |
| A30 | P2: Provider evolution needs typed content and continuation seams | Pending | |
| A31 | P2: Durable work needs an identity beyond a turn or tool call | Pending | |

## Delivery checkpoint

PR [#54](https://github.com/Boredphilosopher96/Rottweiler/pull/54) merged the first integrated batch. Candidate6c3a4ae3853a9793fd0b7c2b04565aa7b66b2d36 passed every hosted gate in run33930648431. Main3ef57e913384bb9f2cd26341c337aa171c786c2f has the identical Git tree; its post-merge CI run33931348188 is still running. The required aggregate is now enforced in the main ruleset, bound to GitHub Actions, with unrelated rules preserved.

The next batch is on `feat/mission-architecture-foundations`. A10 ordered execution windows and A11 JSON sizing are locally verified. A02 segmented journal migration,A04 semantic transcript paging and A06/provider lifecycle remain in isolated implementation/review. Dedicated-runner inventory is empty; no eight-hour qualification is claimed. The remaining architecture findings above are still in scope.
