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
- Startup budget is split: engine ready < 50ms, TUI first paint < 150ms total (Bun-compiled binaries cold-start in tens of ms; opencode demonstrates this is achievable).
- Distribution artifact is larger because a Bun-compiled executable embeds the Bun runtime. The binary-size gate applies to the Rust engine only; the TUI bundle has platform-specific budgets (< 100MB on macOS, < 150MB on Linux) — a knowing trade for the renderer. Linux release builds fail closed unless the copied OpenTUI native library can be stripped with the trusted system `strip` binary.
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

**Decision.** TOON encoding applies to structured/tabular tool results and MCP payloads. Prose (model text, file contents) is never TOON-encoded. Every TOON payload is preceded by a one-line format note the first time it appears in a session. **The format is pinned**: we implement the open TOON specification (the `toon-format` project); the spec text and its reference test vectors are **vendored into `docs/spec/toon/`** as the first task of M3, and the vendored snapshot — not the moving upstream — is what the serializer, round-trip proptests, and benchmarks target. If upstream is unusable for any reason, the fallback is an in-repo grammar defined in that same directory before any encoder code is written; either way, no TOON code exists before its grammar document does.
**Rationale.** TOON's savings are real on uniform arrays (benchmarks ~40–60% vs JSON) but it *hurts* readability/accuracy on deeply-nested or free-text data. Scope it to where it wins; measure (07-VERIFICATION has a token-savings benchmark gate).

---

## ADR-010: Compaction is a 1:1 port of opencode's strategy

**Decision.** Copy opencode's compaction behavior exactly (reference: `opencode/packages/opencode/src/session/compaction.ts` and `overflow.ts`; the constants and flow below are the contract):
1. **Prune** (continuous, free): walk backward; skip the newest 2 user turns; protect the newest `PRUNE_PROTECT = 40_000` tokens of completed tool outputs and a protected-tools list; stop at the last summary/pruned marker; erase older tool outputs only if reclaimable > `PRUNE_MINIMUM = 20_000`.
2. **Overflow check**: fire when total tokens ≥ usable − reserved, `reserved = min(20_000, max_output_tokens)`, both user-configurable (`compaction.auto = false` disables, `compaction.reserved` overrides).
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

**Decision.** The source of truth for ClientCommand/EngineEvent/IR is the Rust types in `rw-types` (annotated with `schemars`); the JSON Schema in `protocol/` and the TypeScript types in `packages/tui` are **generated** from them (schemars export + typeshare-style TS emission). Generated artifacts are committed; CI fails if they're out of sync.

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

**Revisit when.** OpenAI or GitHub publishes a stable first-party raw provider API/SDK that preserves Rottweiler's headless provider boundary, or a compatibility canary fails; migrate deliberately and retain fixture compatibility where possible.

---

## ADR-018: Distribution is one application bundle with one public command

**Decision.** Preserve ADR-001's separate supervised Rust-engine and OpenTUI
processes, but ship them only as one complete platform bundle. `rw` is the sole
public entrypoint. Homebrew is the primary distribution: the versioned macOS
Cask stages the exact release archive in Homebrew's managed directory, while
the versioned Formula keeps the same private engine, TUI, WASM host, and native
renderer files together under `libexec`. Both expose one package-manager-aware
`rw` wrapper in `PATH`. A generated
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

**Decision.** Rottweiler ships Wasmtime's component-model runtime in a private `rottweiler-wasm-host` helper beside `rw` and exposes one versioned, length-bounded string/JSON hook ABI. This is still one installed application and one public `rw` entrypoint; the helper is never placed on the user's PATH. Keeping Wasmtime out of the normal `rw` dependency graph preserves no-extension startup latency and makes a guest-runtime failure process-isolated. WASM hooks register on the existing `HookDispatcher`, use the same protocol-1 capability manifest and hook names as RPC plugins, and receive a fresh short-lived store for each invocation. The helper clears its environment and provides no WASI and no filesystem, network, process, environment, clock, randomness, or credential imports. Component size, protocol frames, serialized input bytes, linear-memory/table/instance counts, aggregate enabled bytes, fuel, and the complete helper request lifetime are bounded. A helper that stalls during validation, request writes, response reads, or shutdown is killed and reaped before the request fails. Fuel-based async yielding makes dispatcher cancellation observable while guest code is running. The component ABI's owned-string return is checked immediately after canonical lifting; the tighter linear-memory ceiling bounds the unavoidable allocation before that check. Tools, commands, providers, events, and push methods remain on the crash-isolated RPC tier until their component interfaces can preserve the same security and cancellation contracts.

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
