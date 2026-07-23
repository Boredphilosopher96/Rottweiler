# 04 — Extensibility

Goal: pi-grade extensibility — nothing in the harness is magic. The test: **every built-in command, mode, agent, and tool registers through the APIs documented here.** If a built-in needs a private hook, the public API grows until it doesn't.

## Tier 1 — Declarative (no code)

Discovery order (ADR-014), first match by name wins: `.agents/` (project) → `.rottweiler/` (project) → `~/.agents/` (user) → `~/.rottweiler/` (user). The open `.agents` location is primary so your config stays portable across harnesses; project-level artifacts are inert until their project extension inventory is trusted (05-SECURITY Layer 0).

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

Mode files use one bounded schema. The file stem must match `id`; `permission`
selects the built-in permission floor (`discuss`, `plan`, or `execute`), while an
optional non-empty `allowed-tools` list further narrows the session registry.
An empty list leaves the registry unchanged. Project modes remain inert until
the extension inventory containing them is trusted. The security-sensitive
built-in ids `discuss`, `plan`, and `execute` are reserved: a discovered file
using one of those ids fails registry composition instead of changing the named
built-in's permission contract. Custom ids can select any of the three permission
floors.
When a custom mode becomes active, its canonical semantic fingerprint (id,
description, permission floor, prompt, and sorted tool allowlist) is persisted
without its source path. Resume and rewind compare that fingerprint against the
trusted registry and fail closed if the definition was removed or changed.
Legacy fingerprint-free events are accepted only for the three built-in ids.

Interactive clients discover modes through the bounded, connection-scoped
`ListModes`/`ModesListed` protocol catalog. `/mode` with no argument lists the
active mode and every available id; `/mode <id>` selects any registered mode.

```toml
id = "audit"
description = "Inspect the workspace without changing it"
permission = "discuss"
prompt = "Audit claims against repository evidence and do not mutate files."
allowed-tools = ["read", "grep", "glob"]
```

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

**One dispatcher, two adapters:** the engine-internal **hook dispatcher** owns registration, ordering, and fail-open/closed semantics. Built-ins such as `[toolchain]` formatters/linters and permission supplements consume it directly; the RPC bridge exposes that same dispatcher to out-of-process plugins. Both paths use the identical interface (dogfooding rule), and the conformance suite rejects a second hook mechanism.

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

### Executable configuration and approval

Executable configuration follows the normal `.agents`-before-`.rottweiler` discovery rule and is ignored at project scope until its project extension inventory is trusted. Commands are literal argv arrays: shell parsing and `PATH` lookup are never implicit, and the executable must resolve to an absolute executable file.

```toml
# .agents/mcp.toml
[servers.local]
argv = ["/absolute/path/to/mcp-server", "--stdio"]
cwd = "."
defer_tools = true
inherit_env = []
# Empty lists are the fail-closed defaults. Roots resolve relative to this file's
# configuration base and must remain inside an active workspace root.
read_roots = ["src", "docs"]
write_roots = ["generated"]
# Public DNS names only; traffic is forced through the supervised SSRF proxy.
allowed_domains = ["api.example.com"]

[servers.remote]
endpoint = "https://mcp.example.com/rpc"
defer_tools = true
oauth_credential = "mcp.example"
oauth_resource = "https://mcp.example.com/rpc"
oauth_audience = "mcp.example.com"
oauth_authorization_endpoint = "https://id.example.com/authorize"
oauth_token_endpoint = "https://id.example.com/token"
oauth_client_id = "public-client-id"
oauth_scopes = ["mcp:read", "mcp:call"]

# User-level mcp.toml only. Tool entries take precedence over the server default.
[capability_overrides.local]
default = ["reads_fs"]
[capability_overrides.local.tools]
read_document = ["reads_fs"]
publish_document = ["network", "exec"]
```

```toml
# .agents/plugins.toml
[[plugins]]
name = "example"
argv = ["/absolute/workspace/.agents/plugins/example/dist/plugin"]
manifest = ".agents/plugins/example/manifest.json"
inherit_env = []
allowed_domains = []
```

`/mcp` shows connection and approval state. Stdio servers receive only intrinsic runtime reads, scratch writes, and no network by default. `read_roots`, `write_roots`, and `allowed_domains` are bounded explicit process grants; roots must stay within active workspace authority, and domains use the supervised policy proxy with DNS pinning and private/local-address denial. Separately, virtual MCP tool calls classify as `network + exec` unless user-level `capability_overrides` supplies a server default or an exact per-tool override (`reads_fs`, `writes_fs`, `network`, `exec`); project configuration cannot downgrade this permission classification. Tool entries take precedence over the server default. Approval is bound to both kinds of grants together with the exact origin, transport, argv/environment names, OAuth references, and configuration fingerprint; changed configuration requires a new explicit fingerprint confirmation. `rw mcp login <server>` uses Authorization Code + PKCE and atomically stores the access token, optional refresh token, expiry, and exact resource/audience binding in the Rottweiler credential vault. Expired access is refreshed only against the same trusted token endpoint/client/proxy configuration, and a rotated refresh token is durably replaced before the new bearer is exposed. Plaintext tokens and environment-backed MCP OAuth references are rejected. Remote prompts are available through `/mcp.prompt <server> <prompt> [JSON object]`; catalog-derived namespaced aliases are conveniences and the stable command resolves the live server state at invocation.

`rw plugin status|approve|revoke` manages the separately fingerprinted plugin approval ledger. Production approval pins and displays the executable plus every explicit interpreter entrypoint and adjacent dependency descriptor by canonical path, length, and BLAKE3 identity; identities are revalidated immediately before launch, and eval/module/package-runner forms are rejected because their executed content cannot be attested narrowly. A separately pinned `code_root` (the manifest's parent directory) is the only plugin-owned directory readable without `reads-fs`; it must be a strict descendant of an approved workspace root, never the workspace root itself. Omit `cwd` to default it to that code root. The TypeScript production path is the scaffold's `bun run build`, which emits a standalone `dist/plugin`; `bun run start` remains the development path. `rw plugin scaffold --lang ts` emits the canonical protocol-1 TypeScript template and manifest. `rw plugin dev <path> --allow-dev-exec` is an explicit local-development escape hatch: it runs under the restrictive plugin sandbox, watches source files without a build loop, and never grants or mutates production approval.

Protocol 1 is frozen by the plugin conformance suite: the Rust host runs the canonical generated scaffold plus independent tool/hook, event/push, and provider fixtures, and kills an undeclared-capability fixture. Provider plugins emit request-correlated `provider/event` notifications incrementally and receive `provider/cancel` when the consumer drops; their streams are bounded and cancellation-cleaned without a whole-call five-second deadline. Wire details and limits live in `packages/plugin-sdk/PROTOCOL.md` and its checked-in JSON schema.

The protocol documentation site is generated deterministically by
`packages/plugin-docs` from that frozen Markdown, schema, and canonical wire
fixture. It adds searchable navigation and direct schema/fixture downloads
without introducing a second protocol source; CI rebuilds and tests the static
site alongside the TypeScript SDK.

## Tier 3 — WASM hook components

Private Wasmtime component-model helper for hook extensions (ADR-021). It is bundled beside `rw` but is not a second app or public command: `rw` starts it only to validate or invoke an enabled extension. A component exports this versioned WIT surface:

```wit
package rottweiler:extension@1.0.0;

world hook-extension {
  export invoke: func(event: string, payload-json: string) -> string;
}
```

The return value is a bounded JSON directive: `continue`, `replace` with a typed JSON payload, `block` with a user-facing message, or `error`. The component declares protocol-1 hooks through the same `PluginManifest` used by Tier 2 and is registered on the same `HookDispatcher`; ordering, deadlines, cancellation, and fail-open/fail-closed behavior therefore do not fork.

The public `rw` binary does not link Wasmtime. It communicates with the private helper over a one-shot typed, length-prefixed stdio exchange containing the exact signature-verified component bytes; malformed or oversized frames fail closed. There are no host imports and no WASI. Every call uses a fresh store with bounded component and serialized-input bytes, per-memory size, memory/table/instance counts, and fuel. The entire helper exchange, including validation, stdin writes, stdout reads, and process exit, has a fixed deadline; a timed-out or malformed helper is explicitly killed and reaped. Fuel periodically yields from the async Wasmtime call so dispatcher cancellation stops guest execution rather than merely abandoning a blocking worker. Enabled components are also bounded by count and aggregate installed bytes. The owned-string result is checked immediately after canonical lifting; its unavoidable pre-check allocation remains bounded by the store's linear-memory ceiling. This first production slice intentionally rejects tool, command, provider, event-subscription, and push capabilities: those remain RPC plugins until component interfaces can preserve their streaming and permission contracts.

### Signed extension registry

Registry catalogs are bounded refreshable caches, and every entry is validated before it can be printed. Every release binds the exact name, semantic version, capability manifest, HTTPS artifact URL, byte length, lowercase BLAKE3 digest, and publisher public key with an Ed25519 signature. The catalog cannot nominate its own trust: installation succeeds only when the publisher key is supplied independently and pinned in the separate `trusted-publishers.json` approval catalog, and component bytes must match the signed size and digest. A staged install atomically publishes the artifact and signed release record, but never embeds its own trust anchor. Enabling re-verifies the release against the separate publisher pin before displaying capabilities and records lowercase publisher-key, manifest, and component fingerprints in `enabled.json`. Activation names use the same bounded canonical grammar as manifests. Installed files are opened with no-follow descriptor traversal on Unix and bounded opened-handle checks elsewhere. A broken enabled extension is skipped with a control-stripped durable warning so `rw extension disable` remains available for recovery. Workspace-root changes reuse the session's already validated hook generation; they neither reread mutable extension files nor ignore registration conflicts.

`rw extension registry list --catalog <https-url>` fetches a bounded catalog through the hardened signed-update network path. `rw extension registry install <name> --catalog <https-url> --publisher-key <base64>` selects the latest semantic version (or an explicit `--version`), verifies the independently supplied publisher key, signature, URL, size, and digest, then installs it inactive. `rw extension enable <name> <version>` safely reloads the signed local record, displays and confirms the exact manifest, asks the bundled helper to compile the component, and pins its manifest/component fingerprints. `status` shows inactive, enabled, missing, and tampered records explicitly; `disable` removes only activation. A changed installed file is rejected and skipped on the next session start, with a durable warning visible to attached clients.

## Extension of the extension system

- `rw plugin scaffold --lang ts` generates a working plugin skeleton with tests.
- `rw plugin dev <path>` runs a plugin with hot-restart and RPC tracing for debugging.
- Protocol is versioned (`protocol: 1`); engine supports N and N-1.

## What extensions can never do

- Read credentials/API keys from the engine.
- Bypass the permission engine or sandbox (a plugin tool's `caps` manifest is enforced, not trusted).
- See redacted secrets (redaction runs before events reach plugins).
