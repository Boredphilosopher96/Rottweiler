# Mission architecture remediation

Source assessment: [architecture review](2026-09-04-mission-architecture-review.md).
Authorized September 4, 2026: implement all findings, without backward compatibility constraints.

## Execution phases

- [x] Ground: source review, live CI evidence, project contracts.
- [ ] Sketch: compare CI ownership designs, then cross-boundary runtime contracts.
- [ ] Agree: synthesize designs and record decisions. User authorized implementation without an additional checkpoint.
- [ ] Implement: CI stabilization, runtime liveness, journal/client state, scheduling, extensibility, workspace/context, qualification.
- [ ] Reassess: replace designs that fail integration or measurement; no silent scope reduction.

## Status rules

Pending means no completion claim. Source complete needs checked-in implementation and focused verification. Verified needs integrated checks. Hosted qualification requires the exact candidate to execute on the required platforms. Infrastructure unavailable is not a passing result.

## Findings

| ID | Work | Status | Evidence / remaining work |
| --- | --- | --- | --- |
| C01 | P1: Restore a trustworthy main branch and complete gate ownership | Pending | |
| C02 | P1: Contract changes must compile and test every consumer | Pending | |
| C03 | P1: Fix the platform boundary exposed by scaffold generation | Pending | |
| C04 | P1: Protected jobs need real runner capacity and a bounded queue | Pending | |
| C05 | P1: Terminal acceptance and soak need explicit readiness and progress evidence | Pending | |
| C06 | P1: Make performance and artifact failures attributable without weakening budgets | Pending | |
| C07 | P1: Dependency maintenance must reconcile configuration and lockfiles | Pending | |
| C08 | P1: Every failed gate must leave usable evidence | Pending | |
| A01 | P1: Ordered output has a circular wait under saturation | Pending | |
| A02 | P1: Reconnect and history paging repeatedly process the lifetime journal | Pending | |
| A03 | P1: Storage latency still controls actor responsiveness | Pending | |
| A04 | P1: The transcript is a recent tail, not a virtualized history | Pending | |
| A05 | P1: Renderer recycling is normal memory control and loses client state | Pending | |
| A06 | P1: Plugin SDK request handling can block its own replies | Pending | |
| A07 | P1: Extension control is too narrow for the stated mission | Pending | |
| A08 | P2: The runtime boundary still owns terminal presentation | Pending | |
| A09 | P2: Command admission and retry semantics need stronger ownership | Pending | |
| A10 | P2: Tool scheduling is all-parallel or all-serial | Pending | |
| A11 | P2: Context assembly repeats work on unchanged content | Pending | |
| A12 | P2: Spend limits observe usage but do not reserve in-flight cost | Pending | |
| A13 | P2: Checkpoint capture can allocate without a file-size budget | Pending | |
| A14 | P2: Workspace intelligence lacks an aggregate cache and freshness contract | Pending | |
| A15 | P2: The client accepts typed-looking wire data without validating its shape | Pending | |
| A16 | P2: Client memory bounds need bytes and allocation accounting | Pending | |
| A17 | P2: UI interaction policy is centralized without a single state model | Pending | |
| A18 | P2: Rich presentation has no third-party contribution contract | Pending | |
| A19 | P1: Current performance evidence does not prove the mission | Pending | |
| A20 | P2: Performance diagnosis lacks stage-level ownership | Pending | |
| A21 | P1: Cancelling an ordinary plugin request does not settle its effects | Pending | |
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
