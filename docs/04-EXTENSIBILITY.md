# 04 — Extensibility

Goal: pi-grade extensibility — nothing in the harness is magic. The test: **every built-in command, mode, agent, and tool registers through the APIs documented here.** If a built-in needs a private hook, the public API grows until it doesn't.

## Tier 1 — Declarative (no code)

Discovery order (ADR-014), first match by name wins: `.agents/` (project) → `.rottweiler/` (project) → `~/.agents/` (user) → `~/.rottweiler/` (user). The open `.agents` location is primary so your config stays portable across harnesses; project-level anything is inert until the folder is trusted (05-SECURITY Layer 0).

| Kind | Location | Format |
|---|---|---|
| Commands | `commands/*.md` | frontmatter: `description`, `model` (alias), `allowed-tools`, `argument-hint`; body = prompt template (`$ARGUMENTS`, `$1..$n`, `` !`cmd` `` interpolation, `@file` inclusion) |
| Skills | `skills/<name>/SKILL.md` | SKILL.md standard: frontmatter `name`, `description`, optional `allowed-tools`; directory may bundle scripts/resources; lazily loaded |
| Agents | `agents/*.md` | frontmatter: `name`, `description`, `model`, `tools`, `permission-mode`, `max-turns`; body = system prompt |
| Modes | `modes/*.toml` | tool filter + permission overlay + prompt fragment (built-in discuss/plan/execute live in this format, embedded) |
| Workflows | `workflows/*.toml` | DAG of steps: `agent`/`command` refs, `parallel = true`, `on-fail`, artifact passing between steps |
| Shell hooks | `hooks.toml` | one-liner hooks without writing a plugin: `[[hook]] event = "post_tool" matcher = "edit(*.rs)" run = "cargo fmt --check {file}"` — the command's exit code/stdout map onto the hook response (nonzero on a `pre_*` hook = deny with stderr as message). Registered on the same internal dispatcher; trust-gated at project level; this is what Claude Code settings-hooks import onto |
| Toolchain | `toolchain.toml` (or `[toolchain]` in config) | per-language/glob `formatter`, `linters`, `test` commands; registers built-in `post_tool` hooks — after edit/write, formatter runs on the touched file and linter diagnostics append to the tool result. Sugar over the public hook API (dogfooding rule) |
| Themes/keybindings | `themes/*.toml`, `keybindings.toml` | TUI only |

## Tier 2 — RPC plugins (any language)

A plugin is an executable declared in `.rottweiler/plugins.toml`. Rottweiler spawns it and speaks **JSON-RPC 2.0 over stdio** (same wire discipline as LSP/MCP).

### Handshake

Plugin returns a manifest on `initialize`:

```json
{
  "name": "my-plugin", "version": "1.0.0", "protocol": 1,
  "capabilities": {
    "tools": [ { "name": "...", "description": "...", "schema": {...}, "caps": ["reads-fs"] } ],
    "commands": [ ... ],
    "hooks": [ "pre_tool", "post_tool", "session_start" ],
    "providers": [ { "alias-prefix": "custom/" } ],
    "event_subscriptions": [ "ToolCallFinished", "TurnFinished" ]
  }
}
```

Capabilities are permission-gated: the user approves a plugin's capability set on first load (recorded, re-prompted on change) — a plugin that suddenly wants `hooks: [pre_tool]` after an update is a supply-chain signal, not a silent upgrade.

### Hook catalog

Hooks are request/response (can modify/block); events are fire-and-forget. Hook timeout default 5s, configurable; on timeout the engine proceeds per hook's declared `fail-open`/`fail-closed` bit.

**Build ordering (two halves, one API):** the engine-internal **hook dispatcher** — the registration points, ordering, and fail-open/closed semantics — exists from M2, because built-ins consume it (`[toolchain]` formatters/linters in M6, permission supplements in M5). What M8 adds is the **RPC bridge** exposing that same dispatcher to out-of-process plugins. Built-ins register through the identical interface the bridge forwards to (dogfooding rule); M8 must not introduce a second hook mechanism.

| Hook | Can do |
|---|---|
| `session_start` / `session_end` | inject context, load state |
| `user_prompt_submit` | rewrite/augment/block the prompt |
| `pre_tool` | allow / deny (with message) / rewrite args — org policy lives here |
| `post_tool` | rewrite result, append diagnostics (e.g. run linter after edit) |
| `pre_compact` | inject context strings or replace the summary prompt (the ADR-010 contract, verbatim) |
| `turn_end` | trigger side effects (notifications, CI) |
| `permission_check` | supplemental decision for `ask`-tier requests (enables auto-approvers) |

Plugins can also *push*: `session/inject_message`, `session/set_status`, `ui/notify` — enough to build things like pi's live extensions (todo watchers, budget guards, custom status widgets).

### Provider plugins

A plugin can register a provider (capability `providers`): the router forwards IR requests for matching aliases over RPC. This keeps "endless extensibility" true even at the model layer (custom gateways, exotic providers) without touching core.

### SDKs

Official plugin SDKs: **TypeScript first** (npm `@rottweiler/plugin`), Rust second (a crate wrapping the protocol). The protocol doc + JSON schema is the source of truth; SDKs are conveniences.

## Tier 3 — WASM (post-v1)

wasmtime component-model host for latency-critical in-process hooks. Same capability manifest, same hook names. Not in v1 (ADR-003).

## Extension of the extension system

- `rw plugin scaffold --lang ts` generates a working plugin skeleton with tests.
- `rw plugin dev <path>` runs a plugin with hot-restart and RPC tracing for debugging.
- Protocol is versioned (`protocol: 1`); engine supports N and N-1.

## What extensions can never do

- Read credentials/API keys from the engine.
- Bypass the permission engine or sandbox (a plugin tool's `caps` manifest is enforced, not trusted).
- See redacted secrets (redaction runs before events reach plugins).
