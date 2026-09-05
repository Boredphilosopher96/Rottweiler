# 03 — Decision Log (ADRs)

Format: context → decision → rationale → revisit-when. The implementing agent must not silently deviate; write a superseding ADR first.

---

## ADR-001: TUI = OpenTUI (TypeScript client), engine = Rust — the opencode split

**Context.** The brief mandates OpenTUI for the TUI and Rust for the harness. OpenTUI is a **TypeScript** library whose rendering core is native Zig (double-buffered cell diffing, damage-tracked partial redraws, GPU-adjacent optimizations) — those renderer optimizations are the point, and no Rust TUI library (ratatui included) has them. OpenTUI cannot be called from Rust in-process without embedding a JS runtime in the engine.

**Decision.** Do exactly what opencode does: **the TUI is a separate OpenTUI/TypeScript process** (`packages/tui`, run on Bun) that talks to the **Rust engine** over the ADR-002 protocol (HTTP + SSE on a localhost/unix socket). The `rw` binary (Rust) is the entry point: it starts the engine in serve mode and spawns the TUI client. The TUI ships as a self-contained executable via `bun build --compile`, so the user still installs one thing and never needs Node/Bun installed.

**Rationale.**
- The user gets the real OpenTUI renderer, not an imitation — and the opencode-quality feel is a hard requirement.
- The Rust engine stays pure Rust: zero JS in-process, all agent logic/tools/router/sandbox unaffected. Print mode, serve mode, and the SDK remain single-binary Rust with no Bun involvement.
- This is a proven production architecture (opencode: TS/OpenTUI frontend + server-mode core), and ADR-002 was already designed for it — the TUI is just the first out-of-process client.
- Side benefit: TUI extensions/themes can be written in TypeScript, the same language as the plugin SDK.

**Consequences (accepted).**
- Two build toolchains (cargo + bun) and a protocol contract test suite between them (see 07 §2).
- Startup has three independently named budgets: engine ready < 50ms, the TUI process-start splash < 150ms total, and an accepted composer keystroke after transcript paint < 500ms. The splash is feedback, not evidence that the application is interactive.
- Distribution artifact is larger because a Bun-compiled executable embeds the Bun runtime. `contracts/release-contract.json` owns the platform-specific engine and TUI bundle budgets. Linux release builds fail closed unless the copied OpenTUI native library can be stripped with the trusted system `strip` binary.
- If the TUI process dies, the engine survives; `rw` reattaches (this must be a tested behavior, not an accident).

**Revisit when.** OpenTUI publishes a stable C ABI / Rust binding for its Zig renderer — then evaluate moving the TUI in-process behind the same component API.

---

## ADR-002: Headless engine + client protocol (client/server-capable)

**Decision.** `rw-core` is a headless engine communicating via `ClientCommand`/`EngineEvent`. In-process channels for print mode and the SDK; `rw serve` exposes the same protocol over HTTP+SSE — and the OpenTUI frontend (ADR-001) is the primary consumer of that transport.

**Rationale.** This one decision buys: the OpenTUI frontend, SDK-for-free, print mode, replay/testing, extension event API, future IDE/web clients, and the discipline that keeps UI concerns out of agent logic. Cost: some ceremony defining events, plus a cross-language contract test suite. Worth it.

**Revisit when.** Never for the split itself; transport choices may evolve (e.g. WebSocket).

---

## ADR-003: Extensibility = config tier + RPC plugins now; WASM later

**Decision.** Three tiers (details in 04-EXTENSIBILITY):
1. **Declarative** — Markdown/TOML: commands, skills, agents, modes, workflows. Ships in v1.
2. **RPC plugins** — out-of-process, any language, JSON-RPC over stdio (LSP/MCP-style), with a capability handshake. Ships in v1. Official TypeScript SDK first (largest plugin-author community — this is how we match pi's extensibility).
3. **WASM components** — in-process wasmtime for latency-critical hooks. Post-v1.

**Rationale.** RPC-over-stdio is proven (LSP, MCP, DAP), language-agnostic, crash-isolated, and trivially sandboxable. Embedding a scripting language (Lua/Rhai/QuickJS) was rejected: picks a winner language, weaker isolation, and pi's ecosystem shows authors want their own language + npm packages. WASM deferred: component-model DX is still rough; RPC latency (sub-ms locally) is fine for hooks that gate tool calls.

**Revisit when.** A hook demonstrably needs <100µs latency, or wasm component tooling matures.

---

## ADR-004: Async runtime — tokio

**Decision.** tokio, multi-threaded runtime; actor-per-session.
**Rationale.** Ecosystem lock-in of reqwest/hyper/rmcp; alternatives (smol, async-std) buy nothing here. Actor-per-session gives single-writer state without lock soup.

---

## ADR-005: Config = TOML; prompts/agents/commands = Markdown with frontmatter

**Decision.** Machine config in TOML (`config.toml`, `mcp.toml`, `models.toml`); anything a human writes prose in is Markdown + YAML frontmatter.
**Rationale.** TOML is the Rust-ecosystem norm and diff-friendly; JSON-with-comments needs a nonstandard parser; YAML's footguns are legendary. Markdown+frontmatter matches the emerging standard across Claude Code/pi/opencode, so people can port their commands/agents with minimal edits.
**Revisit when.** An open interop standard for agent config lands — adopt it.

---

## ADR-006: Session store = JSONL event log + SQLite index, checkpoints = content-addressed blobs

**Status.** Physical event layout and read-integrity policy superseded by ADR-029.
SQLite search projections and content-addressed workspace checkpoints remain in force.

**Decision.** As stated. Not "everything in SQLite," not git-based checkpoints.
**Rationale.** JSONL is append-only-crash-safe, human-inspectable, trivially exportable, and the natural shape of an event-sourced session. SQLite does what it's good at (list/search). Shadow-git for checkpoints was rejected: interferes with user repos, slow on large trees; content-addressed blobs of *touched files only* are O(changes).

---

## ADR-007: Sandbox = OS-native primitives, per-command policy

**Decision.** macOS Seatbelt (`sandbox-exec` profile), Linux Landlock + seccomp (bubblewrap fallback when unavailable). Three-way command classification: **safe-list** (read-only, run sandboxed without asking), **ask** (default), **deny-list**. Network egress blocked inside sandbox by default, with a proxy escape hatch per-domain. Details in 05-SECURITY.
**Rationale.** Matches the brief ("sandbox for only some commands") and current best practice (Codex CLI, Claude Code sandbox). Container-based isolation rejected for v1: heavy, breaks local toolchains.
**Revisit when.** Windows-native support demanded (currently: warn + no sandbox on Windows, WSL recommended).

---

## ADR-008: Provider-blind router with role aliases and data-driven pricing

**Decision.** Features reference model *roles* (`big/fast/plan/compact/title`); config maps roles → provider/model chains; pricing/caps live in a refreshable `models.toml`, seeded from models.dev.
**Rationale.** Provider-blindness is a tenet; role indirection lets each internal function get the right model without hardcoding — subagents/titling default cheap (`fast`/`title`), while the `compaction` agent binds to the `compact` alias, which is **unset by default → falls back to the session's model** (matching opencode's behavior, ADR-010); users who want cheap compaction set the alias. Pricing as data because prices change monthly and hardcoded tables rot.

---

## ADR-009: TOON for structured tool output, not for conversation text

**Decision.** TOON encoding applies to structured/tabular tool results and MCP payloads. Prose (model text, file contents) is never TOON-encoded. Every TOON payload is preceded by a one-line format note the first time it appears in a session. **The format is pinned**: we implement the open TOON specification (the `toon-format` project); the spec text and its reference test vectors are vendored into `crates/rw-context/spec/toon/`, and that snapshot — not moving upstream content — owns serializer, round-trip property-test, and benchmark behavior.
**Rationale.** TOON's savings are real on uniform arrays (benchmarks ~40–60% vs JSON) but it *hurts* readability/accuracy on deeply-nested or free-text data. Scope it to where it wins; measure (07-VERIFICATION has a token-savings benchmark gate).

---

## ADR-010: Compaction is a 1:1 port of opencode's strategy

**Decision.** Copy opencode's compaction behavior exactly (reference: `opencode/packages/opencode/src/session/compaction.ts` and `overflow.ts`; the constants and flow below are the contract):
1. **Prune** (continuous, free): walk backward; skip the newest 2 user turns; protect the newest `PRUNE_PROTECT = 40_000` tokens of completed tool outputs and a protected-tools list; stop at the last summary/pruned marker; erase older tool outputs only if reclaimable > `PRUNE_MINIMUM = 20_000`.
2. **Overflow check**: fire when total tokens ≥ context window − reserved. The default reserve is `min(20_000, max_output_tokens, context_window / 2)`. `compaction.auto = false` disables automatic compaction, and `compaction.reserved` overrides the default. A zero window or an explicit reserve that exhausts the window makes the resolved policy invalid and disables automatic compaction.
3. **Summarize as a message**: a dedicated `compaction` agent (own model alias, falls back to session model) produces the summary with opencode's template — Goal / Instructions / Discoveries / Accomplished / Relevant files & directories — phrased as a hand-off prompt for the next agent, user's language, no tools, media stripped. The summary is an assistant message flagged `summary: true`; context assembly restarts from it.
4. **Provider-overflow replay**: when the request itself exceeded the provider limit, compact everything before the last real user message and replay that message after the summary.
5. **Auto-continue**: after auto-compaction, inject the synthetic "Continue if you have next steps, or stop and ask for clarification" user message (hook-suppressible).
6. `pre_compact` hook may inject context or replace the prompt.

**Rationale.** The user has explicitly validated this strategy in daily use and asked for parity; it's field-proven and its shape (summary-as-message, prune-before-summarize, replay-on-overflow) composes cleanly with our event-sourced sessions. Our additions — pinned AGENTS.md/plan re-entering after the summary, cache-stable summary position — are additive and must never alter the base behavior.

**Revisit when.** Benchmarks (07 §4 post-compaction Q&A) show a measurable continuity win from a changed template — then diverge deliberately, with the benchmark as evidence.

---

## ADR-011: MCP via `rmcp` (official Rust SDK); deferred tool loading is default-on

**Decision.** Use `rmcp` for client and server roles. MCP tools contribute name+description only until first use (schema fetched via built-in `tool_search`), default-on, per-server opt-out.
**Rationale.** Don't hand-roll a protocol with an official SDK. Deferred loading directly answers the brief's "context overload" complaint — typical MCP configs burn 10–50k tokens of schemas that are mostly never called.

---

## ADR-012: Errors, licensing, naming

- Error handling: `thiserror` in libraries, `miette` for user-facing diagnostics in the CLI. No `anyhow` in library crates (typed errors are part of the SDK contract).
- License: Apache-2.0 (SDK adoption friendly).
- Binary name: `rw` (fast to type), project name Rottweiler.

---

## ADR-013: Protocol types are Rust-first, schema is generated

**Decision.** The source of truth for ClientCommand/EngineEvent/IR is the Rust types in `rw-types` (annotated with `schemars`); the JSON Schema and TypeScript contract in `protocol/` are **generated** from them (schemars export + typeshare-style TS emission). `packages/tui/src/protocol.ts` re-exports that generated TypeScript contract. Generated artifacts are committed; CI fails if they're out of sync.

**Rationale.** Schema-first tooling for Rust is mediocre (weak enum/discriminated-union codegen); Rust-first with export gives the same cross-language guarantee minus the toolchain fight. The engine is the protocol's owner anyway.

**Constraint + de-risk.** Protocol enums are restricted to codegen-friendly shapes (struct variants with named fields, internally-tagged serde, no tuple variants, no untyped `Value` where a type is known). **M0 includes a mandatory spike**: run the chosen toolchain over the real `Block`/`ToolOutput`/event enums and round-trip fixtures through Rust and TS *before* any other M0 work is considered done — if the toolchain can't handle the shapes, the shapes change in M0, not in M4.

**Revisit when.** A third non-generated consumer (e.g. a Go client) appears and needs schema-first neutrality.

---

## ADR-014: Config discovery — open `.agents` location first, `.rottweiler` second

**Decision.** Everything user-authorable (agents, commands, skills, AGENTS.md, plugin declarations) resolves in this order, first match by name wins:
1. `.agents/` in project → 2. `.rottweiler/` in project → 3. `~/.agents/` → 4. `~/.rottweiler/`
(project shadows user; within each level the open location shadows the harness one). Harness-private state — `config.toml`, `models.toml`, sessions, credentials — lives only under `~/.rottweiler` (XDG-compliant) and `.rottweiler/`.

**Rationale.** Bets on the open agents ecosystem: your commands/agents live in a portable location any harness can read; Rottweiler-specific tweaks go in the harness dir. Both locations are subject to the same folder-trust gate (05, Layer 0).

**Revisit when.** The open standard specifies its own precedence rules — adopt them.

---

## ADR-015: Remote engine is a core requirement; SSH-forwarded socket is the transport

**Decision.** The engine binds a unix socket (or loopback TCP) with a per-engine auth token; `rw --remote <host>` starts/attaches to an engine on the remote host over SSH port-forwarding and runs the TUI locally. No custom network daemon, no non-loopback listening by default.

**Rationale.** Client/server exists precisely so different UIs can attach to the same engine — local vs remote must be the same code path or it will rot. SSH is the transport users already trust and have configured; we inherit its auth, encryption, and firewall story instead of building one. Consequence: the protocol is designed connection-oriented with resync (event sequence ids) from day one, and no event may embed machine-local absolute paths without marking them as such.

**Sequencing guard.** Remote mode ships in M4, one milestone before the sandbox/permission-rules/trust milestone (M5). Until M5 lands, remote sessions force the strictest interactive policy (every mutating tool prompts, no safe-list, no remembered approvals) — a remote engine on a shared host must never be the least-protected configuration.

**Revisit when.** Demand for browser clients requires WebSocket+TLS listening; that's additive.

---

## ADR-016: Code intelligence = tree-sitter index (always on) + LSP (auto-start), both in v1

**Decision.** Two tiers, both v1: a tree-sitter symbol index (zero-config, incremental, powers the `symbols` tool) and LSP client integration (auto-start servers found on PATH; diagnostics-after-edit; go-to-def/references/rename tools). Formatters/linters are not LSP's job here: they load through the declarative `[toolchain]` config, which registers built-in `post_tool` hooks on the public hook API.

**Rationale.** Tree-sitter gives every language a floor with no setup; LSP gives depth where servers exist; hooks give formatters/linters a uniform mechanism that third parties can extend (dogfooding rule: the built-in `[toolchain]` tier is sugar over the same hook registration a plugin would use).

**Revisit when.** Index memory/latency on giant monorepos breaks budgets → make indexing lazy/scoped.

---

## ADR-017: Built-in subscription providers are isolated, pinned compatibility profiles

**Decision.** `openai_codex` and `github_copilot` are the only built-in consumer-subscription profiles in v1. They are direct provider adapters over Rottweiler IR, never nested coding-agent processes. They have separate adapter kinds, credentials, fixed production origins, header policies, capability/model tables, reasoning-signature namespaces, accounting semantics, record/replay fixtures, and live compatibility canaries. Ordinary `openai` and `anthropic` remain API-key providers and cannot consume subscription credentials. ChatGPT uses its own Rottweiler login and credential bundle; Claude subscription login is explicitly not supported. GitHub Copilot uses the audited public Copilot CLI-compatible OAuth device client identity required for the subscription model catalog. Rottweiler performs its own device grant and stores its own token; it does not copy or silently reuse OpenCode, VS Code, Copilot CLI, or `gh` credential caches. All credentials share one versioned owner-private Rottweiler file (mode 0600 on Unix).

**Rationale.** Users already pay for these coding subscriptions and both products expose working native-client paths used by established coding harnesses. Treating them as explicit compatibility profiles provides useful batteries-included defaults without contaminating the provider-neutral IR or pretending undocumented consumer transports are stable public APIs. Isolation, pinned upstream audits, deterministic wire fixtures, and paid/quota live canaries make drift visible. A single owner-private file gives deterministic persistence without any macOS credential store authorization surface.

**Constraints.** ChatGPT tokens may reach only the fixed ChatGPT Codex backend; Copilot tokens may reach only the exact GitHub auth and Copilot API origins. Redirects are disabled. Project config cannot override those origins or client identities. Subscription usage is never mislabeled as ordinary API-key cost. Any unsupported IR field fails before network or has an explicit, tested compatibility mapping. Upstream behavior is pinned and audited as a compatibility contract; Rottweiler still owns its grant, storage, redaction, and request boundaries.

**Revisit when.** OpenAI or GitHub publishes a stable first-party raw provider API/SDK that preserves Rottweiler's headless provider boundary, or a compatibility canary fails; migrate the implementation and fixtures together without retaining the superseded path.

---

## ADR-018: Distribution is one application bundle with one public command

**Decision.** Preserve ADR-001's separate supervised Rust-engine and OpenTUI
processes, but ship them only as one complete platform bundle. `rw` is the sole
public entrypoint. Homebrew is the primary distribution: the versioned macOS
Cask stages the exact release archive in Homebrew's managed directory, while
the versioned Formula keeps the same private engine, TUI, WASM host, and native
renderer files together under `libexec`. Both expose one package-manager-aware
`rw` symlink in `PATH`; the executable recognizes canonical Homebrew Cellar and
Caskroom paths when directing upgrades. A generated
HTTPS-only bootstrap is the secondary path and may install only an immutable
tag archive whose exact URL, byte length, and SHA-256 were derived from that
release. Homebrew updates use `brew upgrade`; the signed in-app updater remains
for the official versioned installer and refuses package-managed layouts.
Until Developer ID signing and Apple notarization are configured, the pre-v1
Cask discloses that limitation and removes quarantine in a postflight only
after Homebrew verifies the immutable archive's SHA-256. This transitional
exception keeps the supported one-command Cask usable without weakening the
archive identity check.

**Rationale.** The process split provides crash isolation, headless reuse, and
future clients; it does not justify making users install, launch, or close two
programs. Homebrew can keep runtime helpers private while selecting the
immutable native archive in one command. A source `cargo install` cannot deploy the
Bun-compiled executable and native renderer as a managed sibling tree, so
calling it a full installation would produce a broken interactive app. One
byte-identical executable also cannot span macOS and Linux; one complete bundle
per supported platform, selected automatically, is the honest one-app model.

**Revisit when.** Configure Developer ID signing and Apple notarization, then
delete the quarantine-removal postflight. Also revisit if Cargo gains a secure
standard for installing private runtime assets, or OpenTUI gains a stable
Rust/C ABI that removes the helper process and native-library payload without
weakening ADR-001's renderer requirement.

---

## ADR-019: Live catalogs are authoritative; sanitized provider state may cross the UI boundary

**Status:** accepted 2026-07-12; narrows and supersedes ADR-008 only for model availability and explicit user selection. Role aliases remain the default internal routing abstraction.

**Decision.** Provider-authenticated live discovery is the authority for which concrete models are currently selectable. The refreshable `models.toml` table enriches discovered ids with capabilities and pricing but never invents availability. A short-lived provider-neutral cache may expose bounded concrete ids/display names, capabilities, alias membership, current selection, and a sanitized provider inventory containing only display name, auth interaction kind, boolean authenticated/reachable state, model count, and category-only status. This information contains no credentials or connection configuration. Explicit selection may bind a session to a validated concrete `provider/model`; that binding is durable and must be revalidated/reconstructed on live resume while deterministic replay remains socket-free.

The TUI may initiate provider-neutral OAuth or device-flow interactions through engine commands. Configured inference, discovery, authorization, and token endpoints; pending login objects; OAuth client identity; credential references/values; account ids; token responses; and provider wire errors stay behind the Rust provider/core boundary. The narrow interaction exception is the bounded authorization or device-verification URL, loopback redirect URI, and one-time user code that the human must open or enter. That challenge is connection-scoped only: it is never appended to session or control logs, captured by provider record/replay, returned in reconnect gaps, exported, or retained after completion/cancellation. API-key entry uses a secret-input channel and is not represented as an ordinary setting value.

**Rationale.** Alias-derived menus cannot show newly released subscription models, configured providers with no current alias, authentication state, or a truthful concrete selection. Keeping only sanitized operational state in the protocol preserves provider-blind execution while making the default UI usable. Live discovery also avoids treating a periodically refreshed pricing dataset as an availability registry.

**Revisit if:** a provider cannot return a bounded catalog, the protocol cannot prevent endpoint/error leakage, or a future signed offline catalog can prove availability with equivalent freshness and account entitlement semantics.

---

## ADR-020: Routine prompts are mutation prompts; isolation remains capability-driven

**Status:** accepted 2026-07-13; supersedes the prompt defaults in ADR-007 and ADR-011 without weakening sandbox, trust, fingerprint, or explicit-deny gates.

**Decision.** In a normal local interactive Execute session, the default `ask` policy prompts only for tools that may write the filesystem and shell invocations outside the audited read-only safe-list. Reads, search/glob/list operations, todo bookkeeping, web fetch/search, and non-writing network or execution tools run without a routine permission dialog. MCP tools use the same rule: declared filesystem mutations prompt; other calls remain constrained by the server sandbox and first-use server approval. Explicit deny rules, malformed-request rejection, Discuss/Plan mode restrictions, permission hooks, native sandbox grants, SSRF policy, and plugin/MCP fingerprint approval remain independent fail-closed gates.

`/permissions mode default|strict|auto-safe|yolo` applies a session-local overlay for an ordinary local interactive driver. `yolo` suppresses `ask` outcomes but does not override explicit denies, mode restrictions, malformed-input checks, or sandbox boundaries. A launch-fixed headless or remote-strict policy cannot be weakened by a client command.

The built-in no-prompt shell list is intentionally narrow and implemented as hardened execution plans, not string matching: audited absolute binaries, sanitized environments, read-only sandboxing, and command-specific argument validation for `cat`, `ls`, an installed audited `bat`, `git status`, and `git diff`. Compound or ambiguous shell syntax falls back to the ordinary prompt path.

**Rationale.** Repeated prompts for observation and bookkeeping train users to approve dialogs without reading them and make agent loops unusable. Mutation-focused prompts preserve a meaningful decision point while capability-derived isolation and explicit policy continue to provide the actual security boundary.

**Revisit if:** a supposedly non-writing built-in gains durable external side effects, or a platform cannot enforce the declared isolation strongly enough to keep the streamlined local policy honest.

---

## ADR-021: WASM hook components and a signed extension registry ship now

**Status:** accepted 2026-07-13; supersedes ADR-003's WASM deferral for the hook subset only.

**Decision.** Rottweiler ships Wasmtime's component-model runtime in a private `rottweiler-wasm-host` helper beside `rw` and exposes one versioned, length-bounded string/JSON hook ABI. This is still one installed application and one public `rw` entrypoint; the helper is never placed on the user's PATH. Keeping Wasmtime out of the normal `rw` dependency graph preserves no-extension startup latency and makes a guest-runtime failure process-isolated. WASM hooks register on the existing `HookDispatcher`, use the same current capability manifest and hook names as RPC plugins, and receive a fresh short-lived store for each invocation. The helper clears its environment and provides no WASI and no filesystem, network, process, environment, clock, randomness, or credential imports. Component size, protocol frames, serialized input bytes, linear-memory/table/instance counts, aggregate enabled bytes, fuel, and the complete helper request lifetime are bounded. A helper that stalls during validation, request writes, response reads, or shutdown is killed and reaped before the request fails. Fuel-based async yielding makes dispatcher cancellation observable while guest code is running. The component ABI's owned-string return is checked immediately after canonical lifting; the tighter linear-memory ceiling bounds the unavoidable allocation before that check. Tools, commands, providers, events, and push methods remain on the crash-isolated RPC tier until their component interfaces can preserve the same security and cancellation contracts.

The extension registry is a distribution layer, not a trust root. Each release contains its exact manifest, semantic version, HTTPS artifact location, byte size, BLAKE3 digest, publisher public key, and an Ed25519 signature over all of that metadata. Installation requires the publisher key to be trusted independently of the fetched catalog, verifies the signature and artifact bytes, and atomically publishes the complete manifest/component/signed-release record beneath the extension store. The approved publisher key is pinned separately in `trusted-publishers.json`; it is not accepted from the adjacent installed release record. Activation re-verifies the persisted signature against that separate key pin, rechecks the manifest and artifact, and records the publisher-key, manifest, and component fingerprints before capability approval; installation alone never executes code. Thus a coordinated rewrite of an inactive release record, manifest, and component cannot nominate a replacement publisher. Descriptor-relative no-follow reads protect installed artifacts on Unix, with opened-handle type/size validation on other supported platforms. Invalid installed or enabled records remain visible in `rw extension status`; a corrupt extension is skipped without preventing engine startup, and the durable plugin-status/UI-notification event stream surfaces a bounded control-stripped recovery warning in attached clients. Catalog files are bounded caches; signed release metadata plus the user's separate publisher-key and capability approvals are authoritative. A session retains the exact successfully validated WASM hook generation across workspace-root recomposition, while registration conflicts fail the root change instead of silently disabling hooks.

**Rationale.** The user explicitly reprioritized the previously post-v1 WASM tier and registry. A component-only hook ABI captures the intended low-latency benefit without creating a second extension system or granting ambient machine authority. Independent publisher-key trust prevents a compromised catalog from substituting code, while exact artifact and manifest binding makes updates reviewable.

**Revisit when.** Component-model interfaces for tools/providers can carry streaming, cancellation, permission, and redaction semantics without host imports that weaken this boundary; add those capabilities to the same manifest and dispatcher rather than introducing a parallel host.

---

## ADR-022: Plugin-provider authentication is host-mediated, never credential-passing

**Status:** accepted 2026-08-02.

**Decision.** A plugin provider never receives a credential. It asks the host over its provider RPC to perform the authenticated request, naming a credential reference; the host resolves that reference and sends the request through the guarded provider HTTP path.

**Rationale.** Plugin processes run behind a supervised egress proxy configured from their `allowed_domains` policy and injected through `HTTP_PROXY`/`HTTPS_PROXY`. That proxy cannot add an `Authorization` header to HTTPS traffic: CONNECT reads only a bounded TLS ClientHello to validate SNI, then tunnels encrypted bytes without terminating TLS. Making it authenticate on the plugin's behalf would therefore require a plugin-trusted MITM CA, widening the attack surface substantially for a weaker boundary. The plugin environment is deliberately presentation-only as well: its allowlist rejects names containing `KEY`, `TOKEN`, `SECRET`, or `PASSWORD`.

**Consequences (accepted).** Plugin-provider traffic joins the same guarded HTTP path as built-in providers, so per-provider proxying, retry/backoff, and secret redaction apply uniformly.

Because the host now owns the socket, this decision *enables* recording actual request/response wire frames. **That is not implemented.** Plugin adapters remain pinned to `WireMode::NormalizedReplay` (`crates/rw-runtime/src/extension_runtime.rs:1510`, `crates/rw-ext/src/plugin_runtime.rs:1983`): capturing raw HTTP bytes alone would leave the replay provider unable to interpret an arbitrary plugin-specific wire dialect, so wire-fidelity replay needs a replay-through-plugin design that has not been done.

Consequently the "deterministic replay is sacred" tenet holds for plugin providers only at **normalized-event fidelity**, not wire fidelity. This is a known, accepted limitation of the current implementation, not a property this ADR delivers.

**Revisit when.** Never for raw credential delivery. Revisit only the shape of the narrowly scoped host request/signing RPC if a provider's documented authentication scheme cannot be represented without exposing a reusable secret.

---

## ADR-023: Declarative extension discovery is fail-soft; fail-closed scopes to the artifact

**Status:** accepted 2026-08-02.

**Decision.** A malformed, unreadable, or unsafe declarative artifact is skipped and reported as a diagnostic; it never prevents engine startup. “Fail closed” means that artifact does not load, not that the program does not start.

**Rationale.** Discovery is input handling, not a startup dependency. Propagating a per-artifact error from active-root discovery through session engine construction made one bad file or symlink in `~/.agents/skills/` able to prevent `rw` launching in every repository. Refusing that artifact preserves the safety boundary without turning user-editable configuration into a global denial-of-service switch.

**Decision (untrusted roots).** Failure to fully inventory an untrusted project root degrades that root as a unit: it remains inert, emits a diagnostic, and cannot be granted trust. Any partial inventory is discarded and produces no approvable fingerprint. Third-party repository content must not be able to abort the binary before the user has even received a trust prompt.

**Consequences (accepted).** Diagnostics must identify the refused artifact/root without evaluating its content. User-scope artifacts remain available when an untrusted project root is inert. Trust assessment represents an incomplete root as untrustable and rejects a grant, rather than treating an incomplete inventory as an empty or partially approved fingerprint.

**Revisit when.** Never for process-wide failure on an individual artifact. Revisit inventory bounds or diagnostic presentation only if they prevent safe inspection of legitimate large roots; any replacement must still prevent partial inventories from becoming trustable fingerprints.

---

## ADR-024: Configurable gateways use typed fields; adapter kinds and wire modes remain closed

**Status:** accepted 2026-08-02.

**Decision.** Provider configuration exposes typed gateway extension points: static custom headers and credential-referenced header values, authentication scheme and placement, extra query parameters, extra body fields, and catalog-to-wire model-id mappings. These cover mainstream gateways without Rust changes. `AdapterKind` and `WireMode` remain closed core enums.

**Rationale.** Header/auth placement, parameters, bodies, and model naming are configuration concerns. A wire dialect is not: it determines parsing, error semantics, streaming, recording, and replay. An arbitrary config string for a novel dialect would turn a correctness and replay-determinism boundary into unvalidated data. A genuinely novel wire format belongs behind the plugin-provider protocol, where it can be explicitly versioned and constrained.

**Constraints.** `[providers]` is user-scope-only and ignored from project configuration even after the project is trusted; these gateway fields inherit that boundary. Values that authenticate are credential references, never inline secrets.

**Revisit when.** Open either enum only when a new dialect has a complete, versioned parser plus recorded-wire replay fixtures, cancellation/error semantics, and an evidence-backed reason it cannot be expressed by the plugin-provider protocol or existing typed gateway fields.

---

## ADR-025: Providers remain on the trusted native RPC tier

**Status:** accepted 2026-08-02; narrows ADR-021's future-provider revisit condition without changing its WASM hook or registry decisions.

**Decision.** The WASM component tier continues to reject provider, tool, command, event-subscription, and push capabilities. Third-party providers remain trusted native RPC processes until a component interface can preserve the required streaming and permission contracts.

**Rationale.** The component tier is intentionally capability-scoped and signature-verified, but its current hook ABI is a bounded request/response string/JSON interface. It cannot express a provider's incremental stream, consumer cancellation, bounded backpressure, host-mediated credential operation, wire-frame recording/redaction, and per-request permission contract without introducing host imports that weaken the isolation it exists to provide.

**Consequences (accepted).** “Any provider” currently entails a native-process supply-chain surface rather than the component tier's tighter capability boundary. The signed extension registry already protects signed WASM hook-component distribution; it does not make a native RPC provider a component or remove the native-process trust decision.

**Revisit when.** The component model supplies a versioned provider capability with authenticated host calls that do not disclose credentials, incremental streaming with bounded backpressure and cancellation, recordable/redactable wire framing, and capability permissions that remain enforceable at each host boundary. Move providers through the existing manifest and dispatcher only after a conformance suite proves those contracts.

---

## ADR-026: Release qualification is derived from the major version

**Status:** accepted 2026-08-20.

**Decision.** The release tag remains the immutable publication identity, and
its canonical semantic version selects one closed qualification tier. Every
release requires measured core baselines plus the hosted global, native
platform, package, security, and WSL acceptance gates. A major-zero release
records protected soak, Terminal-Bench, dogfood, and paid provider replay as
`not_claimed_for_pre_v1` and never schedules the self-hosted soak matrix. A
major-one-or-later release requires measured soak baselines and successful
results from every protected v1 gate. Callers cannot select or waive the tier.

The preflight runs the measured core performance graph once, then seals its
readiness and both platform evidence sets into a candidate manifest bound to
the exact source SHA, version, workflow run, and run attempt. The tag workflow
must find a successful preflight for its peeled commit and byte-verify that
manifest plus the retained evidence before it builds publication archives.
Expired, missing, stale-attempt, or mismatched evidence blocks publication.

The tag workflow then admits publication through one hosted aggregate job. For
major zero it requires every common job to succeed and the protected soak job
to be skipped by policy. For major one or later it requires that soak job to
succeed. Failure, cancellation, or any unexpected skipped result blocks
signing. The versioned Cask, Formula, bootstrap, signed metadata, and updater
all consume the same tag-built release archives.

**Rationale.** Bootstrap soak ceilings and offline self-hosted runners cannot
honestly be called measured evidence, but they also should not prevent a
pre-v1 release from making the narrower claims it actually proves. Deriving
the tier from SemVer keeps that narrower path unavailable to v1 while avoiding
a caller-controlled release bypass. Measuring an exact commit twice made
publication depend on unrelated hosted-runner noise even after qualification
had passed. Binding retained preflight evidence to the commit preserves the
performance authority without weakening it or rebuilding publication bytes
outside the tag workflow. Preserving the existing tag publisher and original
archives keeps signing and rerun behavior in one established owner.

**Revisit when.** Protected soak measurements and runners are continuously
available for pre-v1 development, or the project needs preflight-built archives
to become publication inputs rather than authorization evidence.

---

## ADR-027: TypeScript source plugins use a sealed, per-plugin process host

**Status:** implemented 2026-08-22. Registry publication of the SDK remains an
exact-tag release operation.

**Decision.** Rottweiler ships one private, authenticated TypeScript host and
spawns one sandboxed host process per active TypeScript plugin. Production resolves
an inert manifest plus an exact source and locked-dependency graph into a sealed,
content-addressed bundle, then runs that bundle through the existing approval,
`PluginLauncher`, `PluginHost`, capability, and adapter path. `manifest.json` is
the single authored capability declaration and is imported through the SDK's
validating boundary; authority is never discovered by executing unapproved
TypeScript. Live development is a separate session-scoped, actor-owned generation
path with a fixed ephemeral capability ceiling, atomic per-turn registry snapshots,
last-good reload, and production restoration on detach. The generic executable
RPC tier remains supported.

**Rationale.** A release-owned runtime removes the embedded Bun copy from every
plugin without moving JavaScript into the Rust engine or combining unrelated
plugins into one failure and authority domain. Sealing from a two-pass private
snapshot makes source approval describe the bytes that execute. Resolving the new
artifact into the current process contract minimizes the production migration;
actor-owned generations provide the Pi-like development loop without unsafe
mutable registries.

**Specification.** `docs/design/typescript-source-plugin-host.md` defines the
preparation protocol, identity and sandbox rules, failure states, live attachment,
release gates, migration checkpoints, and non-goals.

**Revisit when.** The compiled-host feasibility spike cannot load a sealed external
ESM module under both native sandboxes, or a shared-process design can prove equal
kernel authority and crash containment. Either outcome requires a superseding ADR.

---

## ADR-028: Each contract and feature catalog has one owner

**Status:** accepted 2026-08-22.

**Decision.** Each piece of contract data and each feature catalog has one
hand-maintained owner. A consumer imports that owner or reads a generated
projection. Client preflight checks and transport guards may enforce the same
contract at their boundaries, but they must consume the owned limits and types.
They must not copy the values.

Generated files are committed when a consumer needs them. Each generated file
names its generator, and CI regenerates or checks the projection. Tests may own
invalid samples and non-normative examples. A test fixture is not a second owner
of production defaults or supported values. Docs link to the owner or use a
generated reference when exact values matter.

`architecture/ownership.toml` registers the high-risk boundaries that the
repository can check mechanically. Its entries name the owner, the generator,
the generated outputs, and specific definitions that would reintroduce an old
shadow. The checker does not scan for repeated literals because the same number
can have unrelated meanings.

**Rationale.** Release packaging, update verification, client limits, and feature
catalogs had acquired separate hand-maintained definitions. Some copies drifted
while each local test remained green. Naming one owner makes dependency direction
explicit and lets CI reject the known ways that a second owner can return.

**Consequences.** A migration moves all callers to the owner and deletes the old
definition in the same change. Runtime-specific adapters may supply dependencies
or optional capabilities, but they do not maintain another copy of the common
feature list. The manifest covers only registered boundaries. A passing ownership
check does not prove that every unregistered behavior has one implementation.
Exact Rust and Bun versions use their native root files as owners and a dedicated
checker validates every workflow, package, provisioning script, and README
projection.

**Revisit when.** A registered projection cannot be generated or consumed at its
boundary. Any exception must name the authoritative owner and add a test that
detects divergence.

---

## ADR-029: Session journals use immutable segments and bounded replay views

**Context.** A single lifetime JSONL file makes cursor reads proportional to session
age: the current implementation reads and hashes the entire journal under the
writer mutex before decoding a suffix. History pages retain bounded results but
scan the complete journal. The core sink then returns the whole gap in one vector
and each subscriber retains it. These costs conflict with bounded memory,
responsive interruption and independent read/write progress.

**Decision.** Store the authoritative logical event stream in bounded JSONL
segments: immutable sealed segments plus one active append segment. Preserve
versioned envelopes, contiguous sequences, durable-before-visible publication and
interrupted-tail recovery. Sparse sequence indexes and the segment catalog are
rebuildable projections. A reader owns a view pinned to a committed durable tail;
rotation and later appends cannot change that view. Cursor reads and subscriptions
page through it with explicit byte/event bounds and a bounded descriptor count.
Remove the whole-gap sink interface and migrate all callers; do not retain a
compatibility path for the lifetime-file layout.

The store owns physical layout, safe descriptor opening, writer exclusion,
checksums, indexes and opaque checkpoint storage. Core owns event identity,
subscription ordering and the recovery projection. Runtime supplies blocking I/O
execution and composes recovery with extension, accounting and workspace services.
The existing SQLite listing/search index remains a derived projection. Workspace
checkpoints remain separate from journal recovery snapshots.

**Read integrity.** Normal reads validate the referenced/pinned segment identities,
checksums, record/schema/sequence bounds and the snapshot's prefix identity. Unsafe
paths, symlinks, unexpected links, changed opened descriptors and inconsistent
indexes fail closed. Corruption in unrelated unread historical segments is detected
when those segments are accessed or by an explicit full-integrity verification API
and CLI. A tail read no longer rehashes unrelated historical bytes. This is an
explicit change to ADR-006's implementation-level whole-file validation behavior;
normal reads must not claim to have verified the entire lifetime journal. Full
verification streams all segments with bounded memory and reports its verified
sequence/byte coverage.

Sealed files use `<first:020>-<next:020>-<bytes:020>-<blake3>.jsonl`; `next`
is exclusive. File names form the sparse sequence catalog, and payload checksums
are verified when their segments are read. `active.jsonl` is at most 16 MiB;
ordinary segments seal around 1 MiB, preserving batch atomicity. The store rejects
an existing `events.jsonl` session layout before creating a new journal directory.
Captured views share an append-only catalog and retain an immutable entry count.
Catalog entries occupy fixed-size chunks; rotation never copies prior entries.
Each boundary caches its cumulative byte count and prefix hash, so a historical
prefix reads one boundary segment and searches metadata without copying or folding
the lifetime catalog. Page readers clone only the segment descriptor they are about
to read, then release the catalog lock before filesystem access. Offline capture
still enumerates the bounded filename catalog once.

Acceptance harnesses observe the public segmented format and track file identities
across rotation; observing an active record alone does not prove its fsync.

**Read ownership and subscription ordering.** Each runtime host owns an explicit
`JournalReads` service rooted in a pinned storage-directory descriptor. Active
append owners register one committed-prefix publication; duplicate ownership is
rejected. The publication is separate from the fsync-held append mutex and changes
only after a successful durable append, before live event fanout. Capture clones
one consistent prefix under a short publication lock; readers never wait for the
append mutex. Inactive capture takes and releases the journal ownership lock before
returning its view, so a starting/stopping unregistered writer cannot expose an
uncommitted active tail. Reader admission is shared across that host's sessions;
leases keep admission until blocking read work and its retained views finish.

A subscription installs its broadcast receiver and captures the initial prefix
before returning, even when its caller has not polled replay yet. Initial replay
and lag recovery retain one bounded page, suppress duplicate live events, and
reject future cursors. Attach acknowledges the connection without rebroadcasting
the entire durable gap. Fork copy also consumes pinned pages and can resume an
identical partial child; a differing existing prefix is rejected.

**Recovery snapshots.** Snapshots are derived state bound to an exact durable
journal prefix, with a projection version, session identity, content checksum and
byte bound. Reject incompatible, corrupt, future or mismatched snapshots and
rebuild from authoritative events. Checkpoint live recovery state separately from
lifetime transcript/history: serializing an unbounded conversation or accounting
vector does not establish bounded cold recovery. Historical context, rewind and
inspection use journal cursors and paged projections (A04); the live snapshot owns
only the bounded state needed to resume safely. Measure ordinary checkpoint-based
open separately from an explicit rebuild after missing/corrupt metadata.

**SQLite authority and schema admission.** The shared database admits only the
current accounting and search table definitions. No column backfill, inferred
accounting defaults or legacy uniqueness migration runs on open. Unsupported
accounting layouts fail before write pragmas; explicit search rebuild may recreate
unsupported derived sessions/FTS tables, but it preserves charged entries and
independent authoritative tables. Accounting reconciliation inserts missing
journal facts by identity and rejects conflicts. The rebuild transaction rolls
back search changes on a conflict, and an unreadable database is never replaced
with a partial reconstruction that could discard unknown authority.

**Durability and performance.** Appended events are synchronized before acknowledgement.
Segment seal, index and catalog publication have tested crash ordering; derived
index publication must not add one extra fsync per token. A reader's blocking I/O
and decoding run outside the writer mutex. Reader admission, page memory and open
descriptors are bounded. Append scheduling and batching remain the separate A03
workstream and must preserve the same acknowledgement contract.

**Validation.** Test rotation during reads, crashes at publication boundaries,
partial final records, cursor-ahead rejection, corrupt indexes/segments/snapshots,
unsafe descriptors, prefix validation and full verification. Preserve targeted
acknowledgement ordering across replay and live delivery, including broadcast lag.
Use 10K/100K/1M-event fixtures to report bytes read, retained raw-event memory,
writer-lock hold time, append/interrupt latency and cold-open/rebuild work. Keep
full-replay versus snapshot-plus-tail equivalence tests for interrupted operations,
compaction, rewind, mode changes and accounting.

**Revisit when.** A platform filesystem offers a verifiable immutable-object
primitive that makes stronger whole-history integrity checks inexpensive, or
measured workloads justify a different bounded segment/page size. Change those
policies through explicit validation rather than restoring whole-gap reads.

---

## ADR-031: Bounded plugin operations own their effects through settlement

**Status:** accepted 2026-09-04.

**Decision.** Protocol 3 gives host commands correlated typed outcomes and gives
streaming operations explicit bounded delivery and finite host-issued lifetimes.
The reader routes responses and control independently of application handlers.
Admitted host commands own their actor reply and settlement permit until the
actual operation completes; a caller deadline never abandons that ownership.
Transport admission, data bytes, control frames, pending outcomes, and active
operations all have aggregate bounds. Stream completion has reserved storage.
Consumption returns delivery credit; producing progress cannot extend a total
deadline. Progress is observation, not authorization or proof of settlement.

Tool calls carry a validated host-issued `OperationLifetime`: immutable total and
renewable idle deadlines, both monotonic and capped at five minutes. Defaults are
five minutes total and ninety seconds idle. Typed `tool/progress` observations
renew only idle time; hook, catalog, command and lifecycle requests keep their
five-second control contract. Progress has a separate bounded delivery lane,
coalesces to one pending observation per admitted operation, and is rate limited.
Its writer must settle before the final RPC response. Host-owned invocation IDs
bind starts, output, diffs, approvals and final outcomes even when a provider
reuses its call ID. Client progress has no durable sequence and cannot grow the
journal. Recovery finishes an existing invocation or emits a paired start and
finish when committed IR never reached execution admission.

`rw-operation-contract` owns runtime-independent validated lifetime/progress
values shared by tool execution and both wire protocols. Protocol owners generate
TypeScript and schema projections. SDK and client boundary checks preserve the
same semantic count and plain-text constraints.

Native process authority remains the isolation boundary. Cancellation, timeout,
or abandonment of native execution stops admission and tears down the shared
process, reaps it, and drains owned host work before effectful callers finish.
A cooperative cancellation acknowledgement is not proof that native effects
stopped. Slow consumers may pause their stream within its fixed lifetime while
unrelated RPC responses continue; expired or abandoned native operations retain
the conservative process-wide failure domain.

**Alternatives.** A single awaited reader deadlocks nested RPC and couples slow
consumers. Unlimited detached handlers move the deadlock into unbounded memory.
Per-stream cancellation without enforced per-invocation authority cannot prove
effect settlement. A periodically renewed total deadline admits infinite work.

**Consequences.** Rust owns the wire types, limits, and generated SDK projections.
Protocol 2 is removed rather than supported through a compatibility layer. Host
command errors and unknown outcomes are distinct; retrying an unknown mutation
is not automatically safe. Active correlation IDs must be unique. These are
process-bound operations, not durable tasks: disconnect fails pending work and
reconnect requires a fresh process and fresh operation. Durable task recovery
needs a separate actor-owned persistence contract.

**Revisit when.** Host-enforced per-invocation authority can prove independent
cancellation, or a durable operation registry supplies replayable recovery.

---

## ADR-030: History pages contain current semantic transcript rows

**Context.** The client retains a recent event-derived transcript and mounts its
latest sixteen entries. Arbitrary raw event pages cannot reconstruct historical
cards: tool completion and rewinds affect rows outside the page. Native viewport
culling does not restore evicted history or bound client caches.

**Decision.** Core owns a rebuildable semantic transcript projection. Store owns
its indexed persistence and bounded transactions. Runtime admits, schedules and
cancels read work outside the session actor and journal writer mutex. Rust owns
page/content wire types and limits. The TUI owns formatting, measured layout,
selection and anchors. Canonical events remain the sole authority.

Projection version 2 includes a first-class turn-summary row sourced only from
`TurnFinished`, retaining turn identity, status, usage and cost exactly. Provider
call receipts remain audit/accounting facts and do not create display rows.
Summaries use the same ordinal, page-byte and rewind rules as every other row.

A page describes the current effective transcript at an exact applied journal
prefix. Its source item IDs remain stable. Dense semantic ordinals are distinct
from durable event sequences. Structural generation changes when rewind removes
or reorders rows. Item revisions also change for late tool completions, diffs and
associations; unchanged generation does not imply unchanged content. Bounded
invalidations or an explicit cache reset prevent stale historical cards. Clients
reject stale responses and resolve removed item anchors explicitly.

Old semantic snapshots do not remain reopenable indefinitely. Raw journal
prefixes remain available through ADR-029 for content identity, audit and export.
This avoids retaining server descriptors, versioned row bodies or database WAL
for the lifetime of a client view. Normal pages use indexed ordinal/item seeks
and bounded result bytes, without scanning or counting a historical prefix.

Index transactions bound changed rows, input bytes and retained checkpoints.
Publication advances the applied prefix only after every semantic effect through
that prefix is complete. Missing, incompatible or interrupted indexes rebuild
from bounded journal pages. Rewind preserves command and shell history while
removing conversation/tool rows beyond its target. Repacking affected ordinals
can require work proportional to the affected suffix; process it in bounded
transactions and publish the new generation only when complete. Rebuild and
rewind costs must be measured separately from ordinary append/update/page costs.
No extra durable write per streamed token is introduced.

The app owns one charged cache for parent/child transcript pages and content.
Sparse unloaded ranges preserve reachability without a placeholder per lifetime
row. Pinned viewport data, an eviction policy and bounded metadata govern memory.
Native card count and preview bytes are bounded; complete content uses a paged
reader under the same cache. Scroll/reconnect/recycle state is a stable item plus
an offset, never only an absolute scroll position.

**Initial limitation.** Transcript pages do not recover permissions, todos,
accepted input, active operations or other control state. Keep existing live
replay until a complete bounded recovery snapshot replaces it. A04 by itself does
not establish constant-cost initial session attachment or aggregate bounds for
arbitrarily many active operations.

**Validation.** Exercise real mixed 10K-item history, tool changes across page
boundaries, rewinds, large bodies, content reads, eviction/reload and child caches.
Verify first/middle/latest/jump reads, stale responses, removed anchors, resize,
append while scrolled away, reconnect and renderer replacement. Measure indexed
read/update work and rebuild work independently. Assert bytes, descriptors,
mounted cards and per-frame traversal while preserving existing frame budgets.
The detailed contract is in `docs/design/paged-transcript-client.md`.
## ADR-032: Reuse bounded supervised WASM workers

**Status:** accepted 2026-09-04; replaces ADR-021's per-invocation helper process lifetime.

**Decision.** An application-owned `WasmWorkerPool` admits bounded work and lazily
starts private helpers. Hosted sessions share their factory's pool. Each worker
retains one compiled generation keyed by exact component bytes, manifest, limits,
and helper identity. Target, Wasmtime version, and engine configuration are fixed
by that running helper. There is no on-disk executable deserialization cache.
Every call still creates a fresh store and instance with the existing no-import,
fuel, memory, and output restrictions. Queue, load, and execution share an
immutable deadline. Cancelled callers leave cleanup with an owned task; a worker
slot is not reusable until its process is reaped or returned healthy.

**Alternatives.** A helper per plugin retains useful cache affinity but multiplies
idle process memory with installed count. Recompiling in a new helper preserves
isolation but repeats startup and compilation on the hook path. The shared pool
bounds processes independently of plugin count and keeps Wasmtime out of `rw`.
The initial two-worker ceiling is provisional. Worker capacity must follow cold/warm and concurrent measurements; reuse alone
does not establish a performance budget or native Linux qualification.

**Consequences.** The private helper protocol becomes a sequential load/call
protocol and the one-shot API is removed. Traps, protocol failures, and timeouts
retire a worker. Compiled state is immutable; mutable instance state never crosses
calls or sessions. The pool is explicit application state, avoiding process-global
IO objects tied to an unrelated Tokio runtime. Native RPC plugins retain their
separate ambient-effect settlement requirements.


---

## ADR-034: Runtime-owned symbol indexes have a shared byte budget

**Decision.** A runtime owns one workspace index pool. Canonical root and trust
scope identify a shared index; a different worktree or trust scope gets a distinct
index. Child sessions and live root changes use that same pool. Dead registrations
are weak and pruned on admission, with a maximum of 128 live root/scope pairs.

All root indexes share a 64 MiB charge for retained symbol vectors, names, paths
and entry metadata. Per-file and file-count limits also apply to direct updates.
Old revisions are evicted under pressure, and incomplete coverage is reported as
truncated. Query selection retains only the requested best matches and clones
only those results. Native trees and source text do not remain resident between
updates. One parse/read owner per pool bounds transient source and parser work.

**Alternatives.** Keeping a native tree for every indexed file makes source-byte
limits an inaccurate memory contract. A separate budget per session multiplies
memory with session count. Estimating native tree allocations from source length
would claim an unsupported bound. The chosen design keeps content digests and
symbols; unchanged descriptor metadata avoids reads, and unchanged content avoids
parsing. Changed files are parsed afresh. A small measured hot-tree cache can be
added later only with a separate native-memory accounting contract.

**Freshness.** A shared single-flight reconciliation owner scans on first use and
when its two-second freshness interval has elapsed. It checks additions, deletions,
renames and external edits. Unchanged files use descriptor size, modification time
and, on Unix, inode/device/change-time identity. Changed reads compare descriptor
metadata before and after reading; raced reads fail without advancing freshness.
Built-in mutations still update the shared index directly. Monotonic generations
identify content replacement and eviction. Scans and queries run outside async
executor threads in runtime/tool call paths. Each scan admits at most 100,000
entries and 64 directory levels, with partial coverage reported at either cap.
Syntax traversal uses a tree cursor rather than allocating a vector of siblings
for each visited node. No watcher readiness flags live in
individual tools or LSP facades.

**Limits.** This charge is owned index data, not a claim about process RSS or
Tree-sitter allocator overhead. Tree-sitter is temporary and parses at most the
per-file input cap under shared admission. Reconciliation is on demand, so an
external edit may remain cached within the stated freshness interval. LSP process
state remains session-scoped because its sandbox and document state carry separate
authority. Resource-limited symbol results are explicitly partial.

**Validation.** Concurrent-root budget pressure, eviction/drop refunds, trust and
worktree isolation, shared index identity, external edit/add/delete reconciliation,
unchanged generations, bounded ranked queries and production tool composition are
covered by tests. Performance qualification remains separate from these invariants.

---

## ADR-033: Host reads return directly under byte-owned admission

**Status:** accepted 2026-09-04.

**Decision.** The authenticated command channel carries a source-owned read or
control operation. `CommandReply::Read` contains its outcome and typed query
results directly. A read never puts its payload into mutation deduplication or
SSE. The request ledger retains only its authenticated identity and payload hash:
an identical retry reads the current view, and a conflicting reuse is rejected.
Control operations retain their acknowledgement and mutation settlement rules.

The host admits at most eight reads globally and two per client, independently
of 64 admitted control executions. Reads reserve the general 8 MiB encoded reply
ceiling from a 32 MiB aggregate byte budget before executing. Encoding shrinks
the reservation to the retained buffer capacity. The encoded bytes own admission
until the last transport/body clone drops; the function returning does not free
its budget. The general ceiling accommodates existing 5 MiB image previews;
transcript pages and content chunks retain their smaller domain limits.

Rust owns the command class, reply union, limits, event lifetimes and schema.
The TUI validates a reply once at ingress using generated discriminated
validators, checks authenticated request correlation, and forbids durable or
transient events inside a read. Its decoder uses one amortized bounded byte
buffer, so adversarial fragmentation cannot retain a lifetime chunk array.
Direct results cannot advance the durable replay cursor. A session-generation
change or cancelled request discards its late reply.

**Alternatives.** Returning pages through command acknowledgement/SSE retains
large values in unrelated lifetime caches and serializes them more than once.
A second unauthenticated or method-specific history endpoint duplicates identity
and admission rules. Count-only response admission does not account for live
transport buffers after the query function returns.

**Consequences.** Existing host query consumers move directly to typed replies;
there is no old response adapter. Query services remain responsible for bounded
work and decoded data before serialization. The reply reservation bounds encoded
buffers, not arbitrary query implementations or client caches. Mutation cache
ownership, historical projection catch-up, and viewport/cache limits are
separate obligations; direct replies alone do not close A04 or A09.

## ADR-036: Reserve provider work at the shared accounting root

**Status:** Accepted; implementation and production adoption in progress.

**Context:** Checking completed usage before a request allows concurrent sessions or engine processes to spend the same remaining budget. Cancellation and process failure can also leave provider billing ambiguous.

**Decision:** Each logical provider call has a host-owned durable identity, a distinct retry attempt, session/turn ownership, accounting attribution, an injected UTC time, and final input/output bounds. An accounting-root transaction admits its charge against durable usage plus all retained reservations. The engine persists the started transition before invoking the provider. All database work runs outside the engine actor and stays owned if its caller disappears.

The engine first durably appends `ProviderCallAccounted` with the exact call/attempt identity and normalized actual usage and cost. Only that receipt can transfer the reservation into accounted usage. The accounting transaction matches session, turn, attribution, call, and attempt; its source sequence makes retries idempotent and permits later authoritative usage corrections. `TurnFinished` remains a display rollup and cannot settle a call. Dropping a permit never refunds it. Proven unstarted cancellation may release a reservation; ambiguous started calls remain charged through restart until authoritative reconciliation.

Known USD, credit, and subscription-token bounds remain distinct. Unknown pricing or provider behavior is explicitly best-effort. It must not become a zero-price assumption or a strict-cap claim. Admission queues, retained reservations, and plan/actual metadata have fixed bounds. A retry obtains a distinct attempt identity.

**Validation required:** Two independent engine processes competing for one remainder; cancellation during admission/start/terminal writes; crashes; ambiguous failures; exact and conflicting retries; actual usage corrections; mixed billing units; and attribution-specific accounting transfer. No A12 completion claim is made by the interface alone.

Admission totals use a fixed-depth time index over the validated UTC calendar key. A transaction updates separate session and root totals using checked 128-bit integers stored as fixed-width bytes. This preserves all u64 provider charges without SQLite signed-integer overflow or floating-point rounding. Queries exclude future receipts and include unfinished reservations from earlier days. Neither receipt count nor session age changes the maximum lookup depth. Turn-only historical databases cannot prove exact attempt accounting and are refused for new admission without deleting their records.


## ADR-035: Register dormant plugins and own activation through settlement

**Status:** Accepted; implementation proceeds through separately verified preparation
and activation units. **Date:** 2026-09-04.

Session composition reads and validates bounded plugin manifests, then registers
inert tool, hook, command, provider, and event descriptors. It does not launch
native plugins, compile WASM, prepare TypeScript bundles, or eagerly query model
catalogs. The first operation needing an extension activates its immutable
manifest generation. Explicit development attachment remains eager: publication
of its generation requires successful approval and initialization.

The host owns one activation per generation, with bounded process/startup
admission and an immutable monotonic total deadline. Concurrent first uses share
that activation. Exact executable/source identity, approval, workspace roots,
and initialized manifest are checked before the generation becomes ready.
Inert metadata grants no execution authority. Failure is cached for the generation;
a changed configuration creates a new generation instead of retrying implicitly.

Activation owns its subprocesses, preparation jobs, pipes, and cleanup task. A
waiter's cancellation or future drop closes the shared generation while it is
starting. This may fail other first uses of the same native plugin: startup can
create ambient effects, so isolated waiter cancellation cannot honestly prove
settlement. After activation, the existing ordinary-request and provider-stream
settlement contracts remain authoritative.

Source preparation uses owned execution after subprocess launch. Timeout, output
overflow, cancellation, and a dropped caller revoke work and close the pipes,
then require actual process-tree settlement. Sending a kill signal, observing
only the leader's exit, dropping a future, or aborting a Tokio task is not proof.
Admitted ownership remains charged while cleanup is unproven. Completed operation
records retire themselves; cleanup cannot depend on a later request.

A borrowed `OnceCell::get_or_init` future is rejected because caller cancellation
could abandon preparation or initialization. Background eager startup is also
rejected: it still spends startup resources on unused plugins and complicates
approval/error ownership. Resident process limits and concurrent preparation
limits are separate from per-operation RPC/stream admission.

Revisit limits using measured cold and warm activation distributions, memory,
and cancellation latency. Preserve the fixed total deadline, effect settlement,
and independent control/response liveness when changing capacity.

The preparation foundation exposed Bun's ancestor-directory enumeration during
graph construction. macOS preparation grants only exact directory entries; normal
plugin execution does not receive that authority. Landlock's recursive directory
rules cannot represent that grant. Linux must use a controlled preparation
filesystem or hermetic resolver instead of broadening ancestor access. The
native Linux arm64 failure and macOS production checks are recorded in
[preparation evidence](reviews/2026-09-04-architecture-evidence/source-preparation-settlement.md).

Linux preparation now uses `PreparationFilesystem`: immutable, disjoint physical
code, work, mount, and optional output roots. The helper creates a private mount
namespace and exposes code at `/plugin`, owned work at `/scratch`, and output at
`/output`. It binds only declared code and reviewed runtime roots, masks home and
credential paths before binding, mounts a private proc filesystem, and enters the
view before executing the compiler. Directory synthesis is limited to branches
that contain an excluded path: 512 view nodes and 8,192 inspected entries. It does
not copy an entire installed package or recursively grant host ancestors.

The helper pins directory identities before mounting. The preparation layout
carries the approved compiler path, device, inode, length, and BLAKE3 digest.
The helper verifies that identity while copying the opened no-follow file into
an executable memfd, then seals the snapshot against writes and size changes.
It executes that descriptor inside the view. Later changes to the original inode
cannot change the compiler bytes. Snapshot bytes are bounded by the existing
256 MiB executable limit and the two-helper execution admission. Source and runtime mounts
are read-only. Landlock restricts writes to declared output and work directories
plus `/dev/null`. Network access is denied without an egress relay. The helper
removes capabilities and mount-changing syscall authority before compiler exec;
view setup errors fail closed. The preparation owner retains the private view
directory until actual process settlement, including cancellation and panic.
Required user and mount namespaces remain a deployment prerequisite.
