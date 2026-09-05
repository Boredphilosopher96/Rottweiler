# Rottweiler

**A coding agent harness with a Rust engine and an OpenTUI frontend.** The target blend: **opencode's TUI and compaction strategy**, **pi's extensibility**, **Claude Code's CLI orchestration ability** — provider-blind by design.

This file is the entry point for contributors. Product users should start with
the [public documentation site](https://boredphilosopher96.github.io/Rottweiler/).
Read this file fully, then read the maintainer documents relevant to the change.

## Mission

Build the best-performing coding agent harness: instant startup, 60fps TUI, aggressive token economy, and an engine that is headless-first so the TUI, CLI, and SDK are all just clients.

## Core Tenets (non-negotiable, in priority order)

1. **Rust engine, OpenTUI frontend.** All agent logic — session loop, tools, router, sandbox, context engine — is pure Rust with no embedded JS runtime. The TUI is **OpenTUI** (TypeScript, Bun-compiled to a self-contained executable) talking to the engine over the client protocol, exactly like opencode's frontend/core split (ADR-001). Engine ready < 50ms; the process-start splash < 150ms; an accepted composer keystroke after transcript paint < 500ms.
2. **One application.** Engine and TUI remain separate supervised processes internally, but distribution, installation, launch, upgrade, and default shutdown treat them as one product. Users install one complete platform bundle, invoke only `rw`, and never start or manage an engine or TUI helper themselves (ADR-018).
3. **Fast and responsive.** Never block the render loop. Every user action acknowledges in < 16ms. Streaming tokens render as they arrive. OpenTUI's Zig renderer (damage-tracked partial redraws) is the reason it's in the stack — use it properly.
4. **Batteries included.** Compaction, subagent orchestration, plan/discuss/execute modes, model router, cost tracking, MCP, sandboxing — built in, not plugins.
5. **Endless extensibility.** Built-in tools, commands, and modes use the same extension registries third parties get; built-in and RPC providers meet at the same provider abstraction (dogfooding rule). RPC providers can publish bounded model metadata (including thinking/cache capabilities, limits, and pricing) and can name approval-fingerprinted credential references for host-mediated authenticated HTTP without receiving the secret. The model-layer boundary is explicit: built-in adapter kinds and wire modes are closed, third-party provider dialects run as trusted native RPC processes rather than WASM components, and their recordings replay normalized provider events rather than plugin-specific wire frames (ADR-022, ADR-024, ADR-025).
6. **Provider-blind execution.** The engine speaks one internal message IR and providers remain adapters. A bounded, credential-free catalog may cross the boundary for explicit model/provider selection: display names, capabilities, availability, sanitized auth/reachability state, and supported auth interaction kind. Configured inference/API endpoints, credential references or values, proxy details, wire errors, and routing implementations stay inside Rust. The only URL/code exception is a bounded, ephemeral OAuth/device-flow challenge that the user must act on; it is connection-scoped and is never persisted, recorded, replayed, or exported.
7. **Secure by default.** Sandboxed command execution, permission gates, secret redaction, no telemetry without opt-in.
8. **Token-frugal.** Prompt-cache-aware context ordering, TOON/compact encodings for structured data, tool-result pruning, visible cost/context meters.
9. **Open standards.** AGENTS.md, MCP, SKILL.md-style skills, slash commands, session transcripts in an open documented format.

## Document map

| Doc | Contents |
|---|---|
| [packages/docs-site](packages/docs-site) | Public product docs, tutorials, references, and agent projections |
| [docs/01-FEATURES.md](docs/01-FEATURES.md) | Product requirements and behavior |
| [docs/02-ARCHITECTURE.md](docs/02-ARCHITECTURE.md) | System boundaries and ownership |
| [docs/03-DECISIONS.md](docs/03-DECISIONS.md) | Design decisions and rationale |
| [docs/04-EXTENSIBILITY.md](docs/04-EXTENSIBILITY.md) | Extension tiers, plugin protocol, hook catalog |
| [docs/05-SECURITY.md](docs/05-SECURITY.md) | Sandbox design, permission model, threat model |
| [docs/07-VERIFICATION.md](docs/07-VERIFICATION.md) | Test, replay, benchmark, eval, and release gates |

## Rules for the implementing agent

1. **Follow semantic ownership.** Put each fact and feature in one owner. Generate
   or mechanically check projections; never create a second handwritten source.
2. **Validate integrated changes**: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` all pass; `bun test` + typecheck in `packages/tui` and the cross-language protocol contract tests; include the relevant behavioral acceptance tests.
3. **Respect the ADRs** in `docs/03-DECISIONS.md`. Keep the relevant design record, implementation, and tests aligned. Document the design and its rationale directly.
4. **Engine stays headless.** No terminal/UI types in any Rust crate. All UI lives in `packages/tui` (TypeScript/OpenTUI). If the engine needs something shown, it emits an event; if the TUI needs something done, it sends a command. Never a third channel.
5. **Dogfooding rule** (tenet 5): built-in tools, commands, and modes register through the same registries extensions use.
6. **Performance budgets are tests.** The budgets in `docs/07-VERIFICATION.md` are CI assertions, not aspirations.
7. **Deterministic replay is sacred.** Every provider interaction must be recordable and replayable; never add a code path that cannot run under the replay harness. Built-in adapters preserve wire fidelity. RPC provider plugins record and replay normalized provider events.
