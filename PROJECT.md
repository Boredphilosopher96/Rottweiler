# Rottweiler

**A coding agent harness with a Rust engine and an OpenTUI frontend.** The target blend: **opencode's TUI and compaction strategy**, **pi's extensibility**, **Claude Code's CLI orchestration ability** — provider-blind by design.

This file is the entry point for any agent or human working on this project. Read it fully, then read the docs it links to **before writing any code**.

## Mission

Build the best-performing coding agent harness: instant startup, 60fps TUI, aggressive token economy, and an engine that is headless-first so the TUI, CLI, and SDK are all just clients.

## Core Tenets (non-negotiable, in priority order)

1. **Rust engine, OpenTUI frontend.** All agent logic — session loop, tools, router, sandbox, context engine — is pure Rust with no embedded JS runtime. The TUI is **OpenTUI** (TypeScript, Bun-compiled to a self-contained executable) talking to the engine over the client protocol, exactly like opencode's frontend/core split (ADR-001). Engine ready < 50ms; first paint < 150ms.
2. **One application.** Engine and TUI remain separate supervised processes internally, but distribution, installation, launch, upgrade, and default shutdown treat them as one product. Users install one complete platform bundle, invoke only `rw`, and never start or manage an engine or TUI helper themselves (ADR-018).
3. **Fast and responsive.** Never block the render loop. Every user action acknowledges in < 16ms. Streaming tokens render as they arrive. OpenTUI's Zig renderer (damage-tracked partial redraws) is the reason it's in the stack — use it properly.
4. **Batteries included.** Compaction, subagent orchestration, plan/discuss/execute modes, model router, cost tracking, MCP, sandboxing — built in, not plugins.
5. **Endless extensibility.** Everything built-in is implemented *on the same extension APIs* that third parties get (dogfooding rule). If a built-in feature can't be built on the public extension API, the API is incomplete.
6. **Provider-blind.** The engine speaks one internal message IR. Providers are adapters. No provider name appears outside the `providers` crate.
7. **Secure by default.** Sandboxed command execution, permission gates, secret redaction, no telemetry without opt-in.
8. **Token-frugal.** Prompt-cache-aware context ordering, TOON/compact encodings for structured data, tool-result pruning, visible cost/context meters.
9. **Open standards.** AGENTS.md, MCP, SKILL.md-style skills, slash commands, session transcripts in an open documented format.

## Document map

| Doc | Contents |
|---|---|
| [docs/01-FEATURES.md](docs/01-FEATURES.md) | Complete feature spec — the "what" |
| [docs/02-ARCHITECTURE.md](docs/02-ARCHITECTURE.md) | Crate layout, engine design, data flow — the "how" |
| [docs/03-DECISIONS.md](docs/03-DECISIONS.md) | ADRs: every contested choice, with rationale and revisit conditions |
| [docs/04-EXTENSIBILITY.md](docs/04-EXTENSIBILITY.md) | Extension tiers, plugin protocol, hook catalog |
| [docs/05-SECURITY.md](docs/05-SECURITY.md) | Sandbox design, permission model, threat model |
| [docs/06-ROADMAP.md](docs/06-ROADMAP.md) | Phased milestones with acceptance criteria |
| [docs/07-VERIFICATION.md](docs/07-VERIFICATION.md) | How we prove it works: tests, replay, benchmarks, evals |

## Rules for the implementing agent

1. **Follow the roadmap order** in `docs/06-ROADMAP.md`. Do not start a milestone before the previous one's acceptance criteria pass.
2. **Every milestone ends green**: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` all pass — and from M0 on, `bun test` + typecheck in `packages/tui` and the cross-language protocol contract tests — plus the milestone's specific acceptance tests.
3. **Respect the ADRs** in `docs/03-DECISIONS.md`. If an ADR turns out to be wrong, write a superseding ADR explaining why before deviating — never silently diverge.
4. **Engine stays headless.** No terminal/UI types in any Rust crate. All UI lives in `packages/tui` (TypeScript/OpenTUI). If the engine needs something shown, it emits an event; if the TUI needs something done, it sends a command. Never a third channel.
5. **Dogfooding rule** (tenet 5): built-in tools, commands, and modes register through the same registries extensions use.
6. **Performance budgets are tests.** The budgets in `docs/07-VERIFICATION.md` are CI assertions, not aspirations.
7. **Deterministic replay is sacred.** Every provider interaction must be recordable and replayable; never add a code path that can't run under the replay harness.
