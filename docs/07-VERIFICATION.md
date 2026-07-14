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
unit construction. Deterministic loopback tests cover environment-over-private-file API
keys, static OAuth bearer auth, an actual refresh request plus durable rotation,
provider-proxy Basic auth, known-secret fixture redaction, mixed-auth rejection,
and exact model binding. A two-model/one-provider fixture gives the models
different tool capabilities and proves the registry does not collapse them to
one endpoint-wide claim. Missing default aliases and thinking entries pointing
at absent aliases fail before any socket can open.
The refresh fixture deliberately echoes the newly issued bearer token and
rotated refresh token from the model endpoint after the recorder is already
constructed; both are absent from fixture JSON, redactor/runtime diagnostics,
and private-file storage metadata remains secret. A poisoned-registry test proves
previous and subsequent registrations still redact. Catalog tests also bind an
official kind to its canonical namespace despite a conflicting logical-name
entry, while a compatible adapter uses only its explicit logical entry.

The built-in `openai_codex` path has its own deterministic contract suite. It
asserts the exact fixed callback and authorization parameters, PKCE/state, token
exchange and deduplicated refresh, JWT account-id extraction, rotated credential
persistence before bearer exposure, fixed-endpoint and mixed-auth rejection,
required backend headers, subscription-specific Responses body, and dynamic
secret redaction. Factory tests prove its tool/reasoning capabilities do not
depend on API pricing metadata and that dollar pricing remains unavailable.
The live opt-in smoke records and replays a named tool call through the same raw
Responses normalizer; current evidence covers `gpt-5.4-mini`. Unproven models are
not added to the conservative allowlist, and `max_output_tokens` is deliberately
absent because the live subscription backend rejects it.

The `github_copilot` contract is also entirely deterministic in CI. An injected
device-flow transport and clock exercise the verification URI/user code,
polling intervals, slow-down handling, expiry, denial, and cancellation without
opening a browser or reading a real credential backend. Captured `/models`
fixtures cover policy filtering, required capabilities, Messages > Responses >
Chat endpoint selection, nominal AI-credit conversion, and malformed/401/403
failure classes. Factory fixtures use injected credential-store backends and a
test-only loopback runtime; the credential canary is registered before discovery
or inference, API-key and endpoint mixing fail before network, and an offline
`ReplayProvider` consumes the recorded inference without a discovery socket.
Additional adversarial fixtures prove that the outer exact-model binding does
not suppress discovered vision or thinking support, while the inner adapter
rejects each capability when absent before inference. The async provider-neutral
metadata surface returns the authenticated limits, capabilities, `ModelPricing`,
and explicit AI-Credit unit; a hand calculation checks 2,000 input, 500 output,
and 1,000 cached tokens as exactly 1.1 credits at the fixture rates. Credential
fixtures also require the stored issuing client id to match the injected test
identity, mirroring the exact compiled-identity check in production.
An accept-canary listener additionally proves invalid required/named tool choices
return before any `/models` socket. Copilot replay under the process-wide network
deny guard lives in its own integration-test binary, so the global guard cannot
race or contaminate parallel loopback acceptance tests.
Developer builds without the pinned compatible OAuth client identity fail
login with an actionable configuration error; CI never reads another tool's
stored token. The ignored release canary in
`crates/rw-core/tests/live_smoke_credentials.rs` additionally requires that
pinned client identity, an existing device-flow credential, and an
explicit `RW_LIVE_GITHUB_COPILOT_MODEL`; ordinary CI only compiles this path.

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

Attachment acceptance includes cursor-anchored `@` paths with spaces, clipboard and local-path images, removal, rejected-preview/send preservation, async-submit reconciliation, safe workspace traversal rejection, and a real authenticated command POST carrying the maximum legal two-image envelope.
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
- **Mutation-focused permission boundary**: the default-`ask` matrix proves reads, todo, valid public `webfetch`, and non-writing tools never invoke an approver; writes and unsafe Bash do. The hardened `cat`/audited-`bat`/`ls`/`git status`/`git diff` plans are tested against compound syntax, hostile Git/environment overrides, symlinked binaries, mutating flags, and sandbox-write attempts. Session-local YOLO is tested both for suppressed asks and for the explicit-deny/mode/sandbox gates it cannot override; launch-fixed remote/headless policies reject weakening commands.
- **Retained mutation previews**: every tool preview emits a redacted durable `ToolDiffReady` independently of whether permission asked. Rust actor tests cover an auto-approved write with no approval event, protocol fixtures round-trip the event, and the TUI reducer/render tests retain the Tree-sitter-highlighted inline diff through tool completion.
- **Truthful active-service projection**: host tests expose only initialized LSP clients and currently executing formatter/linter guards, never configured-idle commands, arguments, paths, output, endpoints, or credentials. TUI tests poll only while tool work is active and omit empty service/MCP sections.
- **Terminal rendering contracts**: the embedded Tree-sitter smoke parses TypeScript, Bash, and Rust without network/runtime asset lookup; canonical extension fixtures cover the remaining bundled grammars. TUI fixtures retain visible Bash command cards, compact the command palette, and keep unsupported fenced languages as bounded code blocks without claiming a terminal-native diagram renderer.
- **Crash safety**: kill the process at random points during a replayed session → `--resume` always loads a consistent state.
- **Event schema evolution**: old fixture logs (N-1 version) always load.
- **Doctor diagnostics**: injected fixtures independently seed a provider 401/403, a bounded connection failure, unavailable sandbox support, and `TERM=dumb`; each must produce its distinct stable code and a non-zero result. Loopback HTTP fixtures cover rejected API credentials and authenticated explicit-proxy routing. Credential-inventory tests assert two logical references cause exactly one shared vault read and that canary values never occur in text or JSON.

## 3. Performance budgets (CI-enforced, p99 unless noted)

| Metric | Budget | How measured |
|---|---|---|
| Engine ready (serve socket accepting) | < 50ms | hyperfine on release binary, CI perf runner |
| Cold start → TUI first paint (engine + OpenTUI spawn) | < 150ms | same |
| Cold start → prompt ready (with project config + 3 MCP servers deferred) | < 250ms | same |
| Headless print-mode start (pure Rust path, no Bun) | < 80ms | same |
| Input keystroke → echo | < 16ms | TUI latency harness (in-memory terminal, timestamped events) |
| Streaming frame compute (layout + diff + buffer write; the harness measures compute, not display refresh) | macOS: p95 < 16ms, p99.9 < 33ms; Linux: p95 < 40ms, p99.9 < 66ms during 200 lines/s stream into 10MB transcript | stress fixture in TUI harness |
| Engine→TUI event latency over the socket, p99 | < 2ms | contract harness |
| Turn overhead (engine time excluding provider latency) | < 20ms | replay timing |
| Compaction pause (UI blocked) | 0ms (fully async) | assertion: UI events processed during compaction |
| Memory, 8-hour stress session (engine + TUI combined) | < 500MB RSS | soak test, nightly |
| Release size | engine binary < 25MB; TUI bundle < 100MB on macOS / < 110MB on Linux | CI check |

Regression policy: every executable latency and size gate writes integer,
machine-readable metrics and keeps its fixed, platform-specific absolute
budget. macOS is the reference latency platform; Linux may use the explicitly
listed streaming-frame exception above, while input echo, socket latency,
startup, turn overhead, and all macOS budgets remain unchanged. Each platform
suite in `benchmarks/performance-baseline.json` declares `baseline_kind` as
either `bootstrap` or `measured`, plus substantive provenance. A measured value
above 110% of its reviewed measured baseline fails. Schema errors,
missing/duplicate metrics, unknown platforms, bootstrap provenance where
measured provenance is required, and absolute-budget failures cannot be
waived. On pull requests only, a relative regression may be waived when a
maintainer applies the `perf-waiver` label **and** the PR body contains a
`## Perf waiver justification` section of at least 80 characters and 12 words
describing the evidence and tradeoff. Nightly and release jobs never accept
waivers.

The initial checked-in values are explicitly `bootstrap`: core ceilings are
derived from the fixed v1 absolute budgets and the RSS value preserves the
pre-baseline guard. They are not empirical measurements and do not satisfy the
v1 regression gate. Pull-request jobs may use them only as an
absolute-equivalent smoke comparison. Nightly and exact-tag core and soak jobs
pass `--require-measured`, retain their real per-platform JSON observations,
and fail closed until maintainers review that evidence and replace each suite
with `baseline_kind: measured` plus its runner/run provenance. The 10% ceiling
and fixed absolute budgets remain unchanged after calibration.

The memory budget is executable, not an idle sleep. `scripts/run-soak.py`
launches the production supervisor, Rust engine, and compiled OpenTUI together
under a PTY. It submits real accumulating turns through the OpenTUI composer to
a network-free deterministic provider, streams multiple deltas per response,
periodically calls the safe `read` tool, and runs `/compact` against the growing
durable transcript. Each step must appear in the session event log before the
next is submitted. The harness also kills the TUI once and requires the
supervisor to attach a new TUI to the same engine PID with the persisted
transcript intact. It samples combined RSS for the complete supervisor process
tree throughout and fails immediately above 500 MiB. Nightly and tag-release
workflows run it for 28,800 seconds on dedicated self-hosted runners labeled
`soak` for macOS arm64 and Linux x86_64. The resulting JSON is retained and
checked against the platform's measured `soak` suite in
`benchmarks/performance-baseline.json`; bootstrap provenance deliberately
blocks nightly and release completion. Tag-release soaks install and run
the exact already-built archive that publication will sign; nightly soaks build
the current default-branch source locally. Dedicated
runners are required because hosted Actions jobs cannot sustain one continuous
eight-hour process.

**Milestone activation.** A performance budget becomes an executable CI gate in
the milestone that introduces the measured path (print mode in M2, serve/TUI
startup and socket latency in M4, deferred MCP startup in M8). Earlier
milestones must not substitute empty-stub benchmarks that pass without
measuring the named behavior. Once activated, a budget remains in every later
milestone's global gate.

The M8 prompt-ready gate is `crates/rw-cli/tests/m8_release_gate.sh`. It runs the
release `rw` binary with an exact persisted folder-trust inventory and MCP
approval ledger, discovers three project-configured stdio servers, starts each
through the production sandbox launcher, loads their real catalogs, composes
the provider, tools, commands, and session actor, and observes a marker emitted
only at that boundary. The subprocess harness requires three distinct child
PIDs, complete `/mcp status` catalog evidence, and successful child reaping. It
uses an in-memory provider and denied-network MCP sandboxes, so it neither reads
the production credential file nor opens a network connection. After five policy/executable
warmups, release p99 across at least 100 independent fresh-process samples with
identically seeded security state must remain below 250ms; this is explicitly a
warm-cache, fresh-process budget. The isolated Cargo target is deleted on exit.

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

The executable capability lane lives in `evals/`: Harbor 0.18.0 runs the
checked-in 20-task list against `terminal-bench/terminal-bench-2-1@6` through
the normal headless `rw` binary from the exact Linux release archive. The
adapter retains Harbor rewards, stream JSON, and `rw stats --json`; provider
credentials never enter argv. The adapter derives an explicit provider
configuration from the immutable dated model id, so a model alias cannot reach
an unconfigured provider. `scripts/check-terminal-bench.py` requires exactly 20
unique completed trials and compares solve rate, mean tokens, mean wall time,
and mean USD cost against the protected
`ROTTWEILER_TERMINAL_BENCH_BASELINE_JSON` repository variable. Complete Harbor
results are retained for 90 days. Long evals use the dedicated self-hosted
`terminal-bench` Linux runner and never silently skip for a missing opt-in.
`scripts/check-dogfood-gate.py` independently
requires 14 unique consecutive UTC records ending on the release day, with
session evidence and zero P0s. The temporal gate is not satisfiable by a
one-time fixture or a single development run.

## 6. CI pipeline summary

Per-PR: fmt · clippy `-D warnings` · unit+integration (replay, network-denied) · protocol codegen check (schema → generated types are committed and in sync) · `bun test` + typecheck in `packages/tui` · TUI goldens · security tests · perf smoke (startup + latency) · `cargo deny`/`audit` · dependency-direction and guarded-network-boundary checks · docs build.
Nightly: full perf suite · real eight-hour supervised soak with retained baseline evidence · fuzzers · non-optional terminal-bench subset with retained regression evidence · macOS + Linux release matrix · real WSL2 acceptance on a labeled self-hosted runner.
Release: signing and publication depend on the exact tag's global Rust/Bun/docs/supply-chain gates, dedicated native-Ubuntu sandbox/egress acceptance, 14-day dogfood ledger, paid two-family `--record` plus offline replay canary, pinned 20-task Terminal-Bench baseline, macOS/Linux eight-hour soak, WSL2 installation and doctor checks against the exact uploaded Linux release archive, WSL source sandbox checks and DrvFS refusal, reproducible build, provenance attestation, update-signature verification fixtures, and binary-size gates. The release archive is copied byte-for-byte from the Windows-mounted checkout onto the WSL Linux filesystem before extraction and installation. Missing credentials, variables, runners, evidence, or offline public-root inputs leave the release blocked. Offline updater fixtures cover exact-byte metadata tampering, unsigned/wrong-threshold roles, old+new root thresholds, v1→v2→v3 plus persisted-v3→v4 after historical expiry, missing/skipped/root rollback, release metadata/clock rollback, expiry, stable/beta/platform binding, signed downgrade policy, artifact length/hash tampering, archive links/unexpected entries, unsafe/direct-copy layouts, WSL DrvFS, and atomic rollback state. No updater test contacts the public network. `cargo xtask sign-update release` consumes a pre-signed public root chain and release-role mode-0600 seed files only; the separate offline `rotate-root` mode is the only command accepting root private keys.

The long-running `soak`, `terminal-bench`, and `wsl2` labels are operational
security boundaries, not general-purpose shared runners. Repository operators
must provision them as ephemeral or just-in-time runners for a single job,
isolate their runner groups to this repository and workflow set, and destroy or
wipe the machine after the job. Release credentials and paid provider keys must
never be exposed to a persistent runner that can retain a modified checkout,
process, container, credential file, or Docker layer for a later job.

Channel-signing fixtures additionally require stable/beta documents to share one metadata epoch while allowing independent semantic target versions. The first publication is exactly epoch 1; later publications require same-epoch prior documents for both channels and advance exactly to `N+1`. A required fixed signing time rejects expired active roots and new stable/beta specs while allowing authenticated expired priors as historical transition evidence. A beta-only prerelease carries stable forward only from a threshold-valid prior stable envelope; unsigned, cross-channel, split-prior, skipped-epoch, downgraded, URL-mismatched, and unused-artifact fixtures fail before output.
**Network policy**: the socket-deny guard applies to the per-PR test harness; the only networked jobs are the nightly terminal-bench eval (a solve-rate benchmark can't run under replay) and the release `--record` smoke.

`scripts/package-release.py` canonicalizes archive ordering, ownership, modes,
timestamps, and gzip headers under `SOURCE_DATE_EPOCH`. Its checkout-independent
fixture must produce byte-identical archives before a release artifact can be
signed or attested.

The distribution renderer accepts single-link regular release archives only,
requires both the macOS and Linux publication families, and deterministically
emits a Homebrew formula plus bootstrap from their exact bytes. Tests reverse
the input order and require byte-identical output; assert immutable tag URLs,
lengths, SHA-256 values, private `libexec` helpers, the sole public `rw`
wrapper, HTTPS-only redirects, supported-host selection, and rejection of bad
names, duplicates, links, unsupported/missing platforms, length changes, and
digest changes. The pre-release `--HEAD` formula builds both locked Rust and Bun
components and has the same private-helper/public-wrapper layout. Stable release
CI syntax-checks both generated files, attests and publishes them with the
archives, and verifies the Homebrew tap's resulting `main` commit. Release and
soak acceptance must invoke only the installed public `rw` with no TUI path
override, then assert the complete supervisor process tree exits on default
close. Homebrew tests also require `rw upgrade` to direct users to
`brew upgrade rottweiler` rather than mutating the Cellar.

## 7. Definition of Done (any task, any milestone)

1. Code + tests land together; new behavior has a replay fixture or property test.
2. Global gates green; budgets green.
3. Docs updated in the same PR (01-FEATURES if user-visible, 03-DECISIONS if a choice was contested, plugin protocol doc if the API changed).
4. If it fixed a bug: a fixture reproduces the bug and now passes.
5. No `unwrap()`/`expect()` outside tests and provably-infallible spots (clippy lint enforced).
