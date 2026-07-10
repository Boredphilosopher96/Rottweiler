# 02 — Architecture

## Big picture

Headless engine + thin clients. The engine owns all agent logic and speaks an event protocol; the TUI, the headless CLI, and the SDK are peers consuming that protocol. This is the single most load-bearing decision (ADR-002): it forces every capability to be expressible as events/commands, which is also what makes extensibility, replay, and testing tractable.

```
┌──────────────┐  ┌────────────┐  ┌────────────┐
│ packages/tui │  │  rw print  │  │ external   │
│  (OpenTUI,   │  │  mode/CI   │  │ SDK users  │
│  TS on Bun)  │  │  (Rust)    │  │            │
└─────┬────────┘  └─────┬──────┘  └─────┬──────┘
      │   ClientCommand ↓  ↑ EngineEvent
      │   (TUI: HTTP+SSE over localhost/unix socket; print/SDK: in-proc channel)
┌─────┴────────────────┴──────────────┴──────┐
│                 rw-core                     │
│  Session loop · mode state machine ·        │
│  turn scheduler · subagent orchestrator ·   │
│  permission engine · hook dispatcher        │
├──────────┬──────────┬──────────┬───────────┤
│rw-context│ rw-tools │ rw-mcp   │ rw-ext    │
│ budget,  │ built-in │ client + │ plugin    │
│ compact, │ tools +  │ server,  │ host      │
│ TOON,    │ registry │ deferred │ (RPC)     │
│ caching  │          │ loading  │           │
├──────────┴──────────┴──────────┴───────────┤
│               rw-providers                  │
│  message IR · router · adapters · pricing   │
├─────────────────────────────────────────────┤
│                rw-store                     │
│  event log (JSONL) · SQLite index ·         │
│  file checkpoints · config                  │
└─────────────────────────────────────────────┘
         rw-sandbox (used by rw-tools/bash)
```

## Workspace layout

```
rottweiler/
├── Cargo.toml                 # workspace
├── crates/
│   ├── rw-types/              # shared types: message IR, events, config schema, errors
│   ├── rw-store/              # session persistence, checkpoints, config loading
│   ├── rw-providers/          # router, adapters, pricing, auth
│   ├── rw-context/            # token budget, compaction, TOON, cache strategy
│   ├── rw-tools/              # built-in tools + tool registry
│   ├── rw-intel/              # code intelligence: tree-sitter symbol index + LSP client (ADR-016)
│   ├── rw-sandbox/            # OS sandbox profiles + policy classification
│   ├── rw-mcp/                # MCP client/server (rmcp), deferred loading
│   ├── rw-ext/                # extension host: RPC plugins, hooks, commands/skills/agents loaders
│   ├── rw-core/               # the engine: session loop, modes, orchestration, permissions
│   └── rw-cli/                # `rw` binary: arg parsing, print mode, serve mode, spawns the TUI
├── packages/
│   └── tui/                   # OpenTUI frontend (TypeScript, Bun; compiled with `bun build --compile`)
├── protocol/                  # GENERATED (ADR-013): JSON Schema + TS types emitted from rw-types
│                              #   (schemars/typeshare); committed, CI-checked for drift
├── docs/
└── tests/                     # cross-crate integration + replay fixtures + protocol contract tests
```

Dependency rule: arrows point downward only. No Rust crate depends on anything in `packages/`. `rw-types` depends on almost nothing. Enforced by `cargo deny` / a CI check on the dependency graph.

### Process model

`rw` (Rust) is the single entry point. In TUI mode it: binds the engine server to a unix socket (localhost TCP on Windows) with a per-engine auth token, spawns the bundled TUI executable with the socket address + token, and supervises it. Engine and TUI fail independently: TUI crash → `rw` restarts it and reattaches to the live session; engine crash → TUI shows a reconnect state, sessions recover from the event log. Print/serve/SDK paths never touch Bun — headless usage is pure Rust.

**Remote mode** (ADR-015): `rw --remote <host>` SSHes to the host, starts/attaches an engine there, forwards its socket locally, and runs the local TUI against it — same code path as local, which is the point. Two hard rules this imposes everywhere: no protocol message may assume a shared filesystem with the client (file previews/diffs travel in-band), and reconnect/resync is a tested first-class flow, not an error path.

**Resync semantics**: events carry per-session monotonic sequence ids. Live delivery uses the in-memory broadcast channel; **the source of truth for gap replay is the persisted event log** — a reconnecting client sends its last-seen id and the engine streams the gap from disk, so resync is unbounded and immune to broadcast-buffer lag (a lagging live subscriber is dropped to catch-up-from-log mode rather than losing events).

**Concurrent clients**: a session has **one driver and any number of observers**. Only the driver's mutating commands (SendMessage, Interrupt, SwitchMode, approvals) are accepted; observers get the event stream and read-only queries. Driver status is a lease — a new client may take over explicitly (`TakeDriver`), which notifies the old driver; simultaneous mutation conflicts are therefore impossible by construction. **Lease exemptions, stated**: engine-internal injections (compaction auto-continue, doom-loop interruptions) and plugin pushes (`session/inject_message`) bypass the lease — they're session machinery, not clients. Rottweiler-as-MCP-server clients operate only on sessions they created unless they explicitly `TakeDriver` on an existing one; they never silently yank a TUI's lease.

**Supervision & lifetime**: `rw` supervises both children. TUI crash → respawn + reattach. Engine crash → `rw` restarts it; the engine recovers sessions from the event log, marking any in-flight turn `interrupted` (partial provider output is preserved up to the last persisted event; the user is told the turn was cut). Quitting the TUI shuts the engine down by default; `--detach` (and remote mode, always) leaves the engine running for later reattach.

**`!` foreground commands**: the TUI suspends raw mode and hands the real TTY to the child. The engine gates via a protocol pair — `UserShellStarted` puts the session in `UserShellActive` (no agent turn may start), `UserShellEnded { status, captured_output }` releases it. **Local**: `rw` spawns the child on the real PTY directly. **Remote**: `rw` owns the SSH connection, so it opens a *dedicated SSH exec channel with a PTY* (`ssh -t` semantics) to run the command on the remote host, bridges the local terminal to it raw (signals and SIGWINCH propagate), and sends the gate commands to the engine over the normal socket — the engine never owns a terminal in either mode. Signals route to the child.

## Core abstractions

### Message IR (`rw-types`)

```rust
struct Turn { role: Role, blocks: Vec<Block>, meta: TurnMeta }
enum Block {
    Text(String),
    Thinking { content: String, signature: Option<String> },
    ToolCall { id: ToolCallId, name: String, args: Value },
    ToolResult { id: ToolCallId, content: ToolOutput, is_error: bool },
    Image { media_type: String, data: ImageRef },
}
enum ToolOutput { Text(String), Structured(Value /* TOON-encoded on serialization */), Mixed(Vec<...>) }
```

Everything provider-specific (cache_control markers, reasoning-effort params, Gemini quirks) lives in adapter-private extension maps, never in the IR.

IR shape constraint (ADR-013): all protocol-crossing enums use **struct variants with named fields** and typed payloads (no tuple variants, no bare `Value` where a typed shape is known) — the sketches above are illustrative; the M0 codegen spike on the real `Block`/`ToolOutput` types gates the final shapes.

### Engine protocol (`rw-types`)

```rust
enum ClientCommand {
    SendMessage { session, content, attachments },
    Interrupt { session }, ApproveTool { id, decision },
    AnswerQuestion { .. }, SwitchMode(Mode), SwitchModel(Alias),
    Compact { instructions }, Fork { at_turn }, Rewind { to_turn }, ...
}
enum EngineEvent {
    CommandAcknowledged { request_id, session_id?, outcome },
    TurnStarted, TextDelta, ThinkingDelta, ToolCallStarted, ToolApprovalNeeded,
    ToolCallFinished, QuestionAsked, TurnFinished { usage, cost },
    CompactionStarted/Finished, SubagentSpawned/Finished, Error, ...
}
```

Engine events share one delivery channel, but have two explicit scopes.
`CommandAcknowledged` is a connection-scoped control event with a request id,
no session sequence, and an optional session id; it provides the immediate
acknowledgement path without inventing an HTTP-response or UI-only third
channel. It is persisted to the engine control log. Every other event is
session-scoped, carries a per-session monotonic sequence id, is persisted in
that session's event log, participates in reconnect/resync, and is available to
hooks/extensions. One schema, three consumers: UI, storage, extensions.
Versioning uses serde-compatible evolution rules; 64-bit counters and sequence
ids cross JSON as decimal strings so JavaScript clients never lose precision.

### Session loop (`rw-core`)

Single-writer actor per session (tokio task owning session state; commands in via mpsc, events out via broadcast). Turn execution:

1. Assemble context (`rw-context`): **prune runs here, deterministically** — the ADR-010 backward walk executes at the start of assembly (before the overflow check), and each erasure is persisted as a `ToolOutputPruned` event, so live context, `--resume`, `rw replay`, and golden transcripts always agree. User pins join the prune-protected set; user-evicted items count as already-pruned stop markers. Then: stable prefix → conversation → pending queued messages.
2. Stream from router; forward deltas as events.
3. On tool calls: permission check → hook `pre_tool` → (parallel where tools are read-only) execute → hook `post_tool` → results into next iteration. **Determinism rule**: regardless of completion order, tool events are *emitted and logged in tool-call index order* — parallelism is an execution detail, never visible in the event log, so golden-transcript replay stays byte-stable.
4. Loop until no tool calls; fire `turn_end` hooks; reconcile usage/cost; check compaction threshold.

Interrupt = cooperative cancellation token checked at every await point; partial output committed to the log with an `interrupted` marker.

### Mode state machine (`rw-core`)

`Discuss ⇄ Plan ⇄ Execute`. Modes are data, not code: each mode = {system-prompt fragment, tool filter, permission policy overlay}. Defined in the same format extension-provided modes use (dogfooding). Plan→Execute transition requires an approval event carrying the plan artifact.

### Subagent orchestrator (`rw-core`)

Subagent = a full child session with its own event log, restricted tool registry, its own context budget. Parent holds a handle; child events are re-broadcast to the parent's *client* tagged with the child id (TUI shows nested progress) — **display-only, never persisted in the parent's log**. The parent log contains exactly: the `spawn_agent` tool call, `SubagentSpawned`, and `SubagentFinished` + tool result, all in tool-call index order per the determinism rule — parallel children completing in any order cannot perturb it. `rw replay` re-derives nested progress from the child logs by id. Worktree isolation delegates to `git worktree` via `rw-sandbox` path rules.

### Router (`rw-providers`)

```
resolve(alias) → [candidate models] → adapter → provider
```

- Adapters implement `trait Provider { fn stream(&self, req: IrRequest) -> EventStream; fn caps(&self) -> Caps; }`.
- `IrRequest` carries a provider-neutral tool choice (`auto`, `required`,
  `none`, or an exact function name). Adapters validate names locally and map
  the choice to their documented wire shape before any socket is opened.
- `Caps` declares: tool calling, vision, thinking, cache breakpoints, max context. The engine adapts behavior from caps (e.g., no parallel tool hints for models that can't).
- Retry/failover middleware wraps any provider. Pricing table is data (`models.toml`, refreshable), keyed by canonical model id.
- **Record/replay middleware** wraps every provider: `--record` writes request/response fixtures; the replay provider serves them back for tests. This is a provider like any other, which is why nothing can bypass it.
- Provider endpoint, proxy, and credential-reference settings are user-scoped security-sensitive configuration. Project files may select user-defined provider/model candidates, but cannot define or redirect a provider connection.

### Context engine (`rw-context`)

- `ContextAssembler`: builds the prompt with a **frozen prefix** (system, tools, project docs) hashed each turn; a test asserts the hash is stable across turns of the same session unless config changed.
- `Budgeter`: local token estimates per block, reconciled with provider-reported usage; drives meters and the compaction trigger.
- `Pruner` + `Compactor`: **1:1 port of opencode's strategy — ADR-010 is the contract** (backward-walking prune with 40k-token protection window and 20k minimum reclaim; overflow = usable − min(20k, max output); summary generated by the `compaction` agent as an in-conversation assistant message using the Goal/Instructions/Discoveries/Accomplished/Relevant-files template; provider-overflow replay of the last user message; synthetic auto-continue nudge). The `pre_compact` hook contract: inject context strings or replace the summary prompt.
- `toon` module: serializer from `Value` to TOON with property-based tests (round-trip through a decoder) and a token-savings benchmark.

### Storage (`rw-store`)

- `sessions/<id>/events.jsonl` — append-only, fsync'd per turn.
- `control/events.jsonl` — connection-scoped command acknowledgements that do
  not yet have a session log (bounded retention; excluded from session replay).
- `index.sqlite` — session list, titles, costs, full-text search over transcripts.
- `checkpoints/` — content-addressed blobs (BLAKE3) + per-turn manifests of touched files. Rewind = restore manifest.
- Config precedence: built-in defaults ← `~/.rottweiler/config.toml` ← `.rottweiler/config.toml` ← env ← CLI flags. **Exception**: security-sensitive keys (`[permissions]`, safe-list, `[network]`/proxy, telemetry opt-in, update channel) are ignored at project level with a warning (05 Layer 0). Schema in `rw-types`, `rw config check` validates and prints effective config with provenance per key.

**M0 path and merge contract.** `ROTTWEILER_HOME/config.toml` is the explicit
user-path override; otherwise an explicitly set
`$XDG_CONFIG_HOME/rottweiler/config.toml` is used, falling back to the
documented `~/.rottweiler/config.toml`. Environment keys use the `RW_` prefix,
and `rw config check --set key=value` supplies the CLI layer. Tables merge by
leaf, model-alias maps merge by alias, and list values replace rather than
concatenate. Provenance is recorded per rendered leaf. Project-level
security-sensitive sections are rejected before merging, so they never become
effective even transiently.

**M1 provider/network contract.** `[providers.<name>]` holds an adapter kind,
optional endpoint, API-key environment/keychain references, and an optional
provider-specific proxy. `[models].aliases` remains the provider-blind ordered
`provider/model` routing table; `[models].thinking` maps aliases to
`off|low|medium|high`. Proxy authentication is configured as a non-secret
username plus a `proxy_password_credential` identifier; the password resolves
as one logical key inside Rottweiler's single versioned OS-keychain vault (or
warned 0600 fallback) and is never rendered. Production managers share a
process cache. Whole-vault writes serialize through one fixed per-OS-user lock,
fresh-read the durable vault, merge one logical key, then replace it, so separate
CLI/engine processes and alternate config roots cannot lose each other's writes.
`ROTTWEILER_CREDENTIAL_BACKEND=file` prevents every OS-keychain call and selects
the warned fallback explicitly (used by hermetic subprocess tests).
The global `[network]` form uses the same fields. Provider definitions and all
proxy/authentication fields are user-scoped and ignored in project config.
An API key uses `api_key_credential` when configured, otherwise the stable
`providers.<name>.api_key` identifier; `api_key_env` remains the
highest-precedence source. The exact built-in `anthropic` and `openai` adapters
default that environment reference to `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`,
respectively; compatible/custom endpoints never guess an environment name.
`rw auth set-key <provider>` receives the value from a hidden TTY prompt and
passes an opaque core-owned secret directly to the credential manager.

For providers with a documented native OAuth surface, the same user-only table
may set `oauth_authorization_endpoint`, `oauth_token_endpoint`,
`oauth_client_id`, `oauth_scopes`, and optional access/refresh credential-store
identifiers. `rw auth login <provider>` binds `127.0.0.1:0`, prints the external
browser URL, validates the exact callback path and cryptographic state, and
exchanges the code with PKCE `S256`. The token client uses the same resolved
provider/global/environment proxy precedence and separately resolved proxy
credentials as model calls. Access and refresh values go directly to the
credential manager; neither config nor the event/session protocol carries them.
If a token refresh rotates the refresh token, the adapter persists the rotated
value through an injected credential sink before returning the new access token;
storage failure is fail-closed and sanitized.

The `openai_codex` (alias `openai_subscription`) kind is intentionally separate
from standard OpenAI API-key adapters. Its built-in browser flow uses the public
client id, PKCE/state, the exact `http://localhost:1455/auth/callback`, and the
required organization/simplified-flow/originator authorization parameters. One
atomic credential bundle stores access token, refresh token, and the ChatGPT
account id decoded from TLS-acquired JWT claims as an unverified routing hint
(never as authorization or entitlement evidence); Rottweiler never reads
`~/.codex/auth.json`. Calls go only to the fixed ChatGPT Codex Responses endpoint
with bearer, account, originator, versioned user-agent, and random provider-session
headers. Refresh is shared per logical provider and a rotated bundle is persisted
before new bearer material is exposed. The backend request profile uses the
Responses normalizer but moves system text to `instructions`, disables storage,
requests encrypted reasoning content, and omits `max_output_tokens` because the
subscription backend rejects that field. Consequently the harness cannot enforce
a per-request output maximum on this transport; it still reconciles returned token
usage, and exposes no API-dollar pricing for subscription models.

`rw-core::ProviderFactory` is the only production composition root for those
pieces. It resolves provider > global > environment proxy precedence and proxy
credentials once per logical provider, then shares one authentication object
across every model on that endpoint (so concurrent models cannot race OAuth
refresh-token rotation). Authentication precedence is explicit API-key env over
API-key credential; the API-key and OAuth families are mutually exclusive;
within OAuth, an explicitly populated token env wins, otherwise a configured
refresh credential is used, then a stored access token. Missing authentication
is accepted only for an explicitly configured loopback endpoint.
The recorder redactor is a shared registry rather than a construction-time
snapshot: OAuth registers each newly issued access token synchronously before
returning bearer material and registers a rotated refresh token immediately
after durable persistence. Credential-store fallback warnings produced by a
later rotation remain visible through the runtime warning snapshot.

Routing is model-bound rather than pretending endpoint-wide capabilities are
model-wide. Each configured `provider/model` candidate receives a conservative
capability view from the refreshed or bundled catalog, enforces the exact model
id at dispatch, and gets a distinct recording identity/capability manifest.
The router uses private registration keys so multiple models on one logical
provider can truthfully differ. Fields absent from the catalog (currently
vision and an authoritative cache-control mode) remain disabled; catalog
`reasoning_options` effort values are the only basis for enabling a reasoning
dial. Unknown/local models therefore degrade conservatively instead of gaining
features by assumption. Exact built-in kinds always query their canonical
catalog namespace (`anthropic/...` or `openai/...`), regardless of the user's
logical provider name; only compatible adapters use an exact logical
`provider/model` catalog entry. A misleading local name therefore cannot
shadow official capability metadata.

### TUI (`packages/tui`, OpenTUI)

Built on OpenTUI (per ADR-001), which supplies the retained component tree and the Zig renderer (double-buffered cell diffing, damage-tracked partial redraws) — do not reimplement rendering primitives it already provides. Our layer on top:
- **Transcript** as a virtualized list over the session event log — only visible messages mount, so 10k-message sessions scroll at full speed; streaming deltas append to the tail component without re-laying-out history.
- **SSE client** with reconnect + event-sequence resync (events carry monotonic ids; on reconnect the TUI replays the gap from the engine).
- Components: markdown/syntax-highlighted blocks, collapsible tool calls, diff accept/reject view, fuzzy pickers, question prompts, nested subagent progress, status line.
- Types for commands/events are **generated from `protocol/`**, never hand-written — drift between Rust and TS is a build failure, not a runtime bug.

## Cross-cutting behaviors

- **Errors**: `rw-types::Error` with categories (Provider, Tool, Sandbox, Config, Extension); user-facing messages are actionable ("model X hit rate limit, failing over to Y"), full chains in the debug log.
- **Cancellation**: every async boundary takes a `CancelToken`. No detached tasks without a registered owner.
- **Redaction**: a single `Redactor`, with **scoped aggressiveness**: content entering model context (file reads, `bash`/`webfetch` output) is redacted only via *known secrets* (registered env values, keychain entries) and strict key-format regexes — no entropy heuristic there, because false positives corrupt what the model sees and cause wrong edits. The entropy heuristic applies only at export/share boundaries, where a false positive is cosmetic.
- **Time & randomness** injected via traits — required for deterministic replay.
