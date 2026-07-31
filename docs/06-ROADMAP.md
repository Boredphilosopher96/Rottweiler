# 06 — Roadmap

Rules: milestones are sequential; a milestone is done only when its acceptance criteria (AC) pass in CI *and* the global gates pass (`fmt`, `clippy -D warnings`, `cargo test`, performance budgets from 07). Each milestone produces a usable artifact — no milestone ends in scaffolding-only.

Cross-check: every feature in 01-FEATURES.md maps to exactly one milestone below; if you implement something unlisted, update both docs first.

---

## M0 — Skeleton & spine (workspace, types, protocol)

Workspace with all crates stubbed; **codegen spike first (ADR-013): schemars/typeshare over the real `Block`/`ToolOutput`/event enums, round-tripped Rust↔TS — if the toolchain chokes, the IR shapes change here, not later**; `protocol/` generated + drift-checked; `packages/tui` scaffolded (OpenTUI hello-world consuming generated types) **including a go/no-go check of OpenTUI's headless/test surface — if it can't render to an inspectable in-memory buffer, building that harness becomes an M4 line item, budgeted now**; config loading with precedence + `rw config check`; `tracing` wired; CI (fmt/clippy/test/deny/audit + dep-direction check + bun typecheck).

**AC:** `cargo test` green on macOS + Linux CI; generated Rust and TS types compile from the same source and a contract test round-trips fixture events through both; the OpenTUI test-surface decision is written into 07-VERIFICATION; `rw config check` prints effective config with per-key provenance; event schema tolerates unknown fields.

## M1 — Provider layer + record/replay (the testing spine)

Anthropic Messages + OpenAI Chat/Responses-compatible adapters (streaming, tool calls, usage; Gemini v1 routes through Google's official OpenAI-compatible endpoint as defined in 01 §4); router with aliases + fallback chains; auth (API keys + generic documented OAuth plus the reviewed ChatGPT and GitHub Copilot subscription profiles from ADR-017); one versioned owner-private credential file for all logical credential references, with no operating-system credential store calls; direct ChatGPT raw Responses transport; GitHub device flow, Copilot `/models` discovery, and direct Copilot Responses/Chat/Messages transports; **HTTP(S) proxy support — global `[network] proxy`, per-provider override, env-var fallback (01 §4)**; thinking/reasoning-effort dial mapped per adapter; pricing/subscription-quota/AI-credit accounting; **record/replay middleware** (including dynamic model discovery); retry/backoff.

**AC:** live smoke test (recorded once) streams a tool-call turn from both API families through the IR; the separate ChatGPT-subscription smoke uses Rottweiler's credential vault and raw Codex endpoint, forces one named tool, redacts the whole OAuth/account bundle, and replays byte-identically with process networking denied; the GitHub Copilot smoke does the same through a Rottweiler-owned OAuth identity and a currently enabled discovered model; replay harness re-runs all reviewed fixtures byte-identically with network disabled; kill-one-provider test proves failover; proxy fixture: with a local proxy configured globally, all provider traffic routes through it; with a per-provider proxy, only that provider's traffic does (asserted by the proxy's access log); API cost and Copilot AI-credit calculations match hand calculations, while ChatGPT subscription usage is labeled quota/cost-unavailable rather than `$0` API cost.

## M2 — Agent loop, built-in tools, headless CLI

Session actor with the turn loop; **the permission chokepoint** (minimal: capability classes + ask/allow/deny, interactive prompt + `--permission-mode` for headless — M5 enriches it with pattern rules, sandbox, and trust; nothing ever executes around it); **the internal hook dispatcher** (registration, ordering, fail-open/closed — consumed by built-ins from here on; M8 only adds the RPC bridge); **the command registry + dispatch with the built-in commands existing so far** (dogfooding rule: built-ins register like user commands will); tools: read/write/edit/grep/glob/ls/bash (unsandboxed, permission-prompted, **streaming output**), webfetch, todo, ask_user (auto-answer policy in headless); **tree-sitter symbol index + `symbols` tool**; event-sourced persistence + `--resume`/`--continue`; **file checkpoints (content-addressed store) before every mutating tool + basic `/rewind`** — dogfooding a code-mutating agent without undo is how work gets lost; **root AGENTS.md/CLAUDE.md loaded into context** (full nested discovery lands M6); print mode with `--output-format json|stream-json`; **a minimal line-mode REPL** (readline over the in-proc channel — no OpenTUI; this is what makes slash commands, interrupts, queued messages, and `/rewind` actually *usable* before M4, and what makes "start dogfooding here" true rather than aspirational); interrupt & queued messages; **doom-loop detection + max-turns guardrails**.

**AC:** `rw -p "create hello.py that prints hi, run it"` succeeds end-to-end under replay; kill -9 mid-turn then `--resume` continues without corruption; interrupt mid-tool leaves a well-formed log; rewind fixture: 10 edits, `/rewind` to turn 3, workspace byte-identical to the checkpoint manifest; doom-loop fixture (tool failing 5× identically) triggers the stuck-interruption; `symbols` finds definitions across a 3-language fixture repo; a fixture AGENTS.md instruction demonstrably steers the replayed session; **this milestone makes Rottweiler self-hosting-capable for simple tasks — start dogfooding here.**

## M3 — Context engine

Budgeter + live usage reconciliation; stable-prefix assembler with cache breakpoints; TOON serializer; **opencode-parity compaction per ADR-010** (backward-walk pruning with the 40k/20k thresholds, overflow trigger with reserved buffer, summary-as-message via the `compaction` agent + template, provider-overflow replay, auto-continue nudge) plus the additive pinning layer; `/context` + `/cost` breakdowns with **engine-side item eviction/pinning commands** (context surgery — UI lands in M4); per-turn cost events for inline display; **spend-rate alarm + budget caps**; `rw prompt dump`.

**AC:** prefix-hash stability test across 20 replayed turns; TOON benchmark shows ≥30% token reduction on the structured-payload corpus; long-session replay (150-turn fixture) triggers auto-compaction and the post-compaction agent answers a question about pre-compaction file state correctly (golden test); prune fixture: newest two user turns and the 40k protection window untouched, older tool outputs erased only when >20k reclaimable, protected tools never pruned; provider-overflow fixture: last user message replayed intact after the summary; **simulated** cache-hit rate ≥ 80% on the steady-state fixture (via the provider-cache simulator, 07 §4 — provider-*reported* rates are only checkable in the live release smoke); evicting an item via command is reflected in the next assembled prompt (verified via `prompt dump`) and is rewindable.

## M4 — TUI v1 (OpenTUI)

Engine serve mode over unix socket + auth token + process supervision in `rw` (spawn, crash-restart, reattach); **remote mode: `rw --remote <host>` over SSH-forwarded socket (ADR-015) — same code path as local**; OpenTUI app: SSE client with reconnect/resync, virtualized transcript with markdown/syntax highlighting/collapsible tool blocks and **inline per-turn cost**, streaming, diff view with accept/reject, context-surgery UI over the M3 commands, fuzzy pickers (files/commands/models/sessions), status line meters, mode switching UX, themes, **image paste/attach + `@`-file mentions with fuzzy picker**, **`!` TTY handover (agent blocked until child exit, signals to child)**, **$EDITOR prompt composition**, **desktop notifications** (turn finished / approval needed while unfocused); `bun build --compile` packaging embedded in the release artifact. Until M5 lands, remote sessions force the strictest interactive policy (ADR-015 sequencing guard).

**AC:** `rw --remote localhost` (SSH loopback) runs a full session with byte-identical transcript to local mode, both sides pinned to `--permission-mode strict` (the remote strictness guard would otherwise skew approval events); `!python` REPL fixture: agent provably cannot start a turn until the REPL exits, Ctrl+C reaches the child; performance budgets (07): engine ready <50ms, cold start → first paint <150ms, input-echo p99 <16ms, streaming frame-compute p95 <16ms on macOS and <40ms on Linux on the stress fixture (10MB transcript, 200-line/s stream) — measured by the TUI harness in CI; golden-screen tests for the 12 core screens; kill -9 the TUI process → `rw` respawns it and the session reattaches with no lost events (contract-tested via event sequence ids); protocol contract suite green from both sides.

## M5 — Modes, permissions, sandbox, trust

Mode state machine (discuss/plan/execute) with plan-approval gate; permission engine with pattern rules + remembered approvals + `/permissions`; **folder trust gate (05 Layer 0) + `/trust`**; `rw-sandbox` Seatbelt/Landlock with safe-list classification; network egress proxy; redactor at all boundaries; **multi-root workspaces (`--add-dir`)** honored by tools/permissions/sandbox.

**AC:** all security acceptance tests from 05-SECURITY pass (including the untrusted-folder and remote-auth tests); plan mode provably cannot mutate (property test: fuzz tool calls in plan mode, assert zero FS diff); safe-listed `git status` runs with zero prompts, sandboxed; a second root added via `--add-dir` is writable while its parent stays blocked.

> **★ ALPHA CUT.** M0–M5 is the public alpha: fast TUI, compaction, router, sandbox, trust — plus the safety net (checkpoints/`/rewind`) and AGENTS.md/CLAUDE.md reading, both pulled forward into M2 because an alpha without undo or project-instructions isn't shippable to strangers. Known alpha gaps, accepted consciously: no MCP (M8), no custom commands/skills (M6). Ship it, get feedback. Everything after this line hardens and broadens; it must not block first external users.

## M6 — Commands, skills, AGENTS.md, init, toolchain

**User-authored** commands + skills on the M2 command registry (discovery, frontmatter, lazy loading); **`.agents`-first discovery order (ADR-014)**; full AGENTS.md discovery/merging (nested dirs, user level — root-file loading shipped in M2); `/init` and `/deep-init`; memory (`/memory`); **`hooks.toml` declarative shell hooks (04)**; **`[toolchain]` formatters/linters registered on the M2 hook dispatcher**; **`websearch` tool (provider-native or configured API, through the egress policy)**; **LSP integration (auto-start, diagnostics-after-edit, go-to-def/references/rename tools)**.

**AC:** `/init` on three real OSS repos (one Rust, one TS monorepo, one Python) produces AGENTS.md that a fresh session uses to run the correct test command on first try (golden fixtures); `/deep-init` on the monorepo produces per-package files within size budget; a ported Claude Code custom command runs unmodified except frontmatter key renames documented in a migration table; a command in `.agents/commands/` shadows the same name in `.rottweiler/commands/`; edit-a-Rust-file fixture: rustfmt runs and clippy diagnostics appear in the tool result via `[toolchain]`; LSP fixture: introducing a type error surfaces the diagnostic in the same turn without running a build.

## M7 — Subagents & orchestration

Agent definitions; spawn_agent with parallelism/depth limits; nested progress in TUI; worktree isolation; continuable subagents; workflow runner (TOML DAGs).

**AC:** orchestration fixture: parent spawns 3 parallel explorers in worktrees, collates results, main tree untouched (asserted by git diff); **merge-back fixture: a worktree subagent implements a change, its diff artifact applies cleanly to the parent tree, and a deliberately conflicting pair surfaces the conflict in the tool result instead of silently merging**; a workflow `plan → parallel(impl, tests) → review` completes headless under replay; depth/concurrency limits enforced (tests attempt to exceed).

## M8 — MCP + plugin host

rmcp client (stdio + HTTP), deferred tool loading + `tool_search`, `/mcp` runtime controls, size-capped TOON-encoded responses; plugin host: handshake, capability approval, hook catalog, event subscriptions; `rw plugin scaffold/dev`; TypeScript SDK; Rottweiler-as-MCP-server.

**AC:** connect 5 real MCP servers simultaneously — context increase < 2k tokens until a tool is used (deferred-loading proof); scaffolded TS plugin implementing `pre_tool` deny + a custom tool passes the conformance suite; capability-violation test (plugin exceeds manifest) → killed; another agent drives Rottweiler over its MCP server interface. **The plugin protocol is labeled experimental until this milestone's conformance suite exists and three real plugins run against it; only then does `protocol: 1` freeze.**

## M9 — Fork, review, replay, export

(Checkpoints + `/rewind` shipped in M2.) `/fork`; **`/review` session-wide cumulative diff with per-file accept/revert** over the checkpoint store; **`rw replay <session>`** rendering past event logs through the TUI; transcript export md/html/json; session search.

**AC:** fork diverges without corrupting parent; `/review` over a 10-edit fixture shows the exact cumulative diff and reverting one file restores its checkpointed original; `rw replay` of a golden session matches its golden screens; export golden files.

## M10 — Polish & hardening (v1.0 gate)

Background process manager; vim keybindings; `rw stats` (incl. cost attribution: main/compaction/subagents); **`rw import` (Claude Code / opencode / pi: commands, MCP config, CLAUDE.md→AGENTS.md, memory)**; **`rw doctor`**; **self-update (`rw upgrade`, stable/beta channels)**; docs site for the plugin protocol; binary size + startup final tuning; cross-platform pass (macOS/Linux/WSL); fuzzing (config parser, TOON, plugin RPC) in CI.

**AC:** the full eval gate from 07 (terminal-bench subset + two consecutive self-hosting weeks without a P0, per 07 §5); all performance budgets green on both platforms; Rust engine binary < 28MB and TUI bundle < 100MB on macOS / < 150MB on Linux; zero clippy warnings, zero `cargo audit` criticals; `rw import` on a real Claude Code config dir yields working commands + MCP servers + hooks (fixture); self-update rejects the seeded bad-signature fixture and refuses unsigned downgrades; `rw doctor` correctly diagnoses the four seeded failure states (bad key, unreachable provider, no sandbox support, dumb terminal); each release platform produces one complete engine+TUI+renderer archive, the Homebrew and bootstrap metadata deterministically bind those exact bytes, only `rw` is public, and an installed-bundle PTY test proves one launch and one default close leave no owned child.

---

## Post-v1 (explicitly deferred)

IDE integration · web client over `rw serve` · Windows-native sandbox · AWS Bedrock / Google Vertex adapters (v1 route: OpenAI-compatible gateway) · voice · team/shared-session features.

The WASM hook tier and signed extension-registry foundation moved forward under ADR-021: component hooks reuse the production dispatcher with no WASI/imports and bounded execution; signed, independently trusted release installation is the registry trust boundary. Broader component tools/providers remain future work.
