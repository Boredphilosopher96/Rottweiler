# 07 — Verification Strategy

How we know the harness works, stays fast, and doesn't regress. Everything here runs in CI; "the budgets are tests, not aspirations."

## 1. Deterministic replay (the foundation)

The record/replay middleware (M1) is the spine of all agent-level testing:

- `rw --record fixtures/<name>` captures every provider request/response (redacted) into a fixture.
- The replay provider serves fixtures back; **CI runs with network disabled** (a socket-deny guard in the test harness makes accidental live calls fail loudly).
- Time and randomness are injected traits, so a replayed session is bit-reproducible. Parallel tool execution does not break this: events are emitted in canonical tool-call index order regardless of completion order (02-ARCH determinism rule), so the log is order-stable by construction.
- **Golden transcripts**: replayed sessions must produce identical event logs (modulo timestamps). Any intentional behavior change re-blesses fixtures via `cargo xtask bless` with the diff reviewed in the PR.

Fixture library grows with every bug: a fixed bug without a replay fixture reproducing it is not fixed.

### M1 deterministic provider evidence

CI uses wire-faithful loopback origins for Anthropic Messages, OpenAI Chat, and
OpenAI Responses. The production HTTP adapters stream tool calls and usage,
the recorder stores raw SSE, and replay reparses those frames with provider
networking absent before comparing normalized bytes. Separate loopback tests
exercise global/per-provider/authenticated proxies, killed-primary failover,
sticky failback, and the rule that a partial semantic stream can never fail
over. A socket canary also proves that both live adapters reject network-denied
requests before opening a connection. The models.dev converter is covered by
proxy-routed atomic-install tests and a live schema compatibility probe.

OAuth acceptance uses only configurable mock endpoints: deterministic injected
entropy proves RFC 7636 `S256` construction, a real ephemeral loopback listener
proves happy callback and state-mismatch behavior, an authenticated forward
proxy observes the code exchange, and canary assertions cover session/token
debug output plus CLI diagnostics. Refresh-token rotation tests assert that the
rotated value reaches an injected credential sink before bearer material is
returned, and that a storage failure suppresses the access token without
echoing either secret. A CLI integration test reads the printed
authorization URL, sends an adversarial callback state, and verifies that the
process rejects it without echoing the state or authorization code.

The production `rw-core::ProviderFactory` is exercised separately from adapter
unit construction. Deterministic loopback tests cover env-over-keychain API
keys, static OAuth bearer auth, an actual refresh request plus durable rotation,
provider-proxy Basic auth, known-secret fixture redaction, mixed-auth rejection,
and exact model binding. A two-model/one-provider fixture gives the models
different tool capabilities and proves the registry does not collapse them to
one endpoint-wide claim. Missing default aliases and thinking entries pointing
at absent aliases fail before any socket can open.
The refresh fixture deliberately echoes the newly issued bearer token and
rotated refresh token from the model endpoint after the recorder is already
constructed; both are absent from fixture JSON, redactor/runtime diagnostics,
and a keychain-fallback warning remains visible. A poisoned-registry test proves
previous and subsequent registrations still redact. Catalog tests also bind an
official kind to its canonical namespace despite a conflicting logical-name
entry, while a compatible adapter uses only its explicit logical entry.

These deterministic fixtures do **not** substitute for M1's credentialed live
smoke. A minimal tool-call recording from both remote API families remains a
release/milestone gate whenever credentials are available; CI must continue to
replay the reviewed, redacted recordings with external networking disabled.
The single authoritative opt-in harness is
`crates/rw-core/tests/live_smoke_credentials.rs`. It loads user-scoped provider
configuration and credentials through the production factory, preflights both
families before the first paid request, and requires an existing absolute
fixture directory outside the repository, explicit current tool-capable model
ids, and `RW_LIVE_SMOKE=accept-paid-requests` before its ignored test can run.
Keys are supplied with `rw auth set-key anthropic` and `rw auth set-key openai`
(or their configured environment references), never test arguments. The paid
harness forces the exact `live_smoke_ping` function through the
provider-neutral named-tool choice, rather than relying on prompt compliance.
The minimum non-secret user configuration for that harness is:

```toml
[providers.anthropic]
kind = "anthropic"

[providers.openai]
kind = "openai"
```

It belongs in `ROTTWEILER_HOME/config.toml`,
`$XDG_CONFIG_HOME/rottweiler/config.toml`, or the documented fallback
`~/.rottweiler/config.toml`; provider definitions in project config are
security-sensitive and intentionally ignored.

## 2. Test pyramid

| Layer | What | Tooling |
|---|---|---|
| Unit | IR conversions per adapter, TOON round-trip (proptest), permission rule matching, config precedence, redactor, budgeter math | `cargo test`, `proptest` |
| Integration | full turns under replay: tool loops, interrupts, compaction, failover, resume-after-kill | replay harness in `tests/` |
| Protocol contract | fixture ClientCommands/EngineEvents round-tripped through the generated Rust *and* TS types; SSE reconnect/resync scenarios against a mock engine | `protocol/` fixtures, run by both `cargo test` and `bun test` |
| TUI | golden screens (render to an inspectable in-memory buffer, snapshot cells), input latency harness, component tests. Whether OpenTUI provides this surface natively or we build a thin harness is decided by the M0 go/no-go spike; the budget for building it is reserved in M4 | `bun test` in `packages/tui` + `vhs` for visual review artifacts |
| E2E | print-mode runs on real repos under replay; the M6/M7/M8 acceptance fixtures | `tests/e2e/` |
| Security | the acceptance list in 05-SECURITY (sandbox EPERM assertions, canary-string leak fuzzing, injection corpus) | dedicated `security-tests` job |
| Fuzz | config parser, TOON decoder, plugin RPC framing, event-log reader | `cargo fuzz`, nightly job |

### M0 OpenTUI test-surface decision: GO

OpenTUI 0.4.3 exposes a public `@opentui/core/testing` entry point. Its
`createTestRenderer` uses the native renderer with in-memory output and provides
deterministic render flushing, mock keyboard/mouse input, resize control,
character-frame capture, and styled cell/span capture. The M0 proof in
`packages/tui/test/app.test.ts` renders the real application component and
inspects both character and styled-cell buffers. M4 golden-screen and latency
harnesses will build on this surface; no custom terminal renderer is required.

Property tests worth calling out:
- **Plan mode cannot mutate**: fuzz arbitrary tool-call sequences in plan mode → assert zero filesystem diff **outside `.git/` metadata** (read-only-blessed commands like `git status` legitimately refresh the index; workspace content must be untouched).
- **Crash safety**: kill the process at random points during a replayed session → `--resume` always loads a consistent state.
- **Event schema evolution**: old fixture logs (N-1 version) always load.

## 3. Performance budgets (CI-enforced, p99 unless noted)

| Metric | Budget | How measured |
|---|---|---|
| Engine ready (serve socket accepting) | < 50ms | hyperfine on release binary, CI perf runner |
| Cold start → TUI first paint (engine + OpenTUI spawn) | < 150ms | same |
| Cold start → prompt ready (with project config + 3 MCP servers deferred) | < 250ms | same |
| Headless print-mode start (pure Rust path, no Bun) | < 80ms | same |
| Input keystroke → echo | < 16ms | TUI latency harness (in-memory terminal, timestamped events) |
| Streaming frame compute (layout + diff + buffer write; the harness measures compute, not display refresh) | p95 < 16ms, p99.9 < 33ms during 200 lines/s stream into 10MB transcript | stress fixture in TUI harness |
| Engine→TUI event latency over the socket, p99 | < 2ms | contract harness |
| Turn overhead (engine time excluding provider latency) | < 20ms | replay timing |
| Compaction pause (UI blocked) | 0ms (fully async) | assertion: UI events processed during compaction |
| Memory, 8-hour stress session (engine + TUI combined) | < 500MB RSS | soak test, nightly |
| Release size | engine binary < 25MB; TUI bundle < 100MB | CI check |

Regression policy: budgets checked against a stored baseline; >10% regression fails the PR, with a `perf-waiver` label requiring justification in the PR description.

**Milestone activation.** A performance budget becomes an executable CI gate in
the milestone that introduces the measured path (print mode in M2, serve/TUI
startup and socket latency in M4, deferred MCP startup in M8). Earlier
milestones must not substitute empty-stub benchmarks that pass without
measuring the named behavior. Once activated, a budget remains in every later
milestone's global gate.

## 4. Token-economy benchmarks

A corpus of structured payloads (search results, dir listings, MCP responses, diagnostics) with tokenized-size assertions:

- TOON vs pretty JSON: **≥30% reduction** (gate).
- Deferred MCP loading: 5-server config adds **<2k tokens** until first use (gate).
- Cache-prefix stability: byte-identical prefix across turns (gate), plus a **provider-cache simulator** — a model of each provider's breakpoint/TTL rules applied to the actually-assembled request bytes — asserting ≥80% simulated hit rate on the steady-state fixture. (Replay can't measure real cache hits: provider-reported usage in a fixture is frozen at record time, so a regression added later would never show. The simulator catches it; real provider-reported rates are checked only in the live release smoke.)
- Compaction quality: post-compaction Q&A golden tests (agent answers questions about pre-compaction state; graded against expected answers under replay).

## 5. Capability evals (the "best performing harness" claim)

- **terminal-bench subset** (20 tasks) run nightly against a pinned model: track solve rate, tokens, wall time, cost. The harness's job is to not be the bottleneck — compare against a baseline harness (pi or Claude Code) on the same model monthly; regressions in solve-rate-per-dollar are investigated as bugs.
- **Self-hosting**: from M2 onward, Rottweiler development uses Rottweiler. v1.0 gate: two consecutive weeks of dogfooding with zero P0s (data loss, hang, corruption).
- **Compatibility matrix**: ported artifacts (a Claude Code command set, a pi extension rewritten on the plugin SDK, an AGENTS.md-standard repo) exercised in CI as conformance fixtures.

## 6. CI pipeline summary

Per-PR: fmt · clippy `-D warnings` · unit+integration (replay, network-denied) · protocol codegen check (schema → generated types are committed and in sync) · `bun test` + typecheck in `packages/tui` · TUI goldens · security tests · perf smoke (startup + latency) · `cargo deny`/`audit` · dependency-direction and guarded-network-boundary checks · docs build.
Nightly: full perf suite · soak test · fuzzers · terminal-bench subset · macOS + Linux matrix.
Release: reproducible build, provenance attestation, update-signature verification fixtures, binary-size gate, `--record` smoke against live providers.
**Network policy**: the socket-deny guard applies to the per-PR test harness; the only networked jobs are the nightly terminal-bench eval (a solve-rate benchmark can't run under replay) and the release `--record` smoke.

## 7. Definition of Done (any task, any milestone)

1. Code + tests land together; new behavior has a replay fixture or property test.
2. Global gates green; budgets green.
3. Docs updated in the same PR (01-FEATURES if user-visible, 03-DECISIONS if a choice was contested, plugin protocol doc if the API changed).
4. If it fixed a bug: a fixture reproduces the bug and now passes.
5. No `unwrap()`/`expect()` outside tests and provably-infallible spots (clippy lint enforced).
