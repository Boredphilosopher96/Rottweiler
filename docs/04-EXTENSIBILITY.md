# 04 — Extensibility

Goal: pi-grade extensibility — nothing in the harness is magic. Built-in tools,
commands, and modes use the shared registries documented here, and built-in and
RPC providers meet at the same provider abstraction. If a built-in needs a
private hook, the public API grows until it does not.
Provider plugins have the model metadata and host-mediated authentication
needed for first-class routing. Their replay and execution boundaries are
stated below.

## Tier 1 — Declarative (no code)

Discovery order (ADR-014), first match by name wins: `.agents/` (project) → `.rottweiler/` (project) → `~/.agents/` (user) → `~/.rottweiler/` (user). The open `.agents` location is primary so your config stays portable across harnesses; project-level artifacts are inert until their project extension inventory is trusted (05-SECURITY Layer 0).

> **Durable discovery contract:** malformed, unreadable, or unsafe declarative
> artifacts are skipped and reported as diagnostics. They must never prevent
> Rottweiler from starting. Here, *fail closed* means that the individual
> artifact does not load—not that discovery aborts the program. An untrusted
> root that cannot be inventoried completely is discarded as a unit, receives
> no fingerprint, and cannot be granted trust. A regression in this contract is
> a bug.

| Kind | Location | Format |
|---|---|---|
| Commands | `commands/*.md` | frontmatter: `description`, `model` (alias), `allowed-tools`, `argument-hint`; body = prompt template (`$ARGUMENTS`, `$1..$n`, `` !`cmd` `` interpolation, `@file` inclusion) |
| Skills | `skills/<name>/SKILL.md` | SKILL.md standard: frontmatter `name`, `description`, optional `allowed-tools`; directory may bundle scripts/resources; lazily loaded |
| Agents | `agents/*.md` | frontmatter: `name`, `description`, `model`, `tools`, `permission-mode`, `max-turns`; body = system prompt |
| Modes | `modes/*.toml` | tool filter + permission overlay + prompt fragment (built-in discuss/plan/execute live in this format, embedded) |
| Workflows | `workflows/*.toml` | DAG of steps: `agent`/`command` refs, `parallel = true`, `on-fail`, artifact passing between steps |
| Shell hooks | `hooks.toml` | one-liner hooks without writing a plugin: `[[hook]] event = "post_tool" class = "transform" failure_policy = "fail-closed" matcher = "edit(*.rs)" run = "cargo fmt --check {file}"` — the command's exit code/stdout map onto the hook response (nonzero for a policy hook blocks with its diagnostic). Registered on the same internal dispatcher; trust-gated at project level; this is what Claude Code settings-hooks import onto |
| Toolchain | `toolchain.toml` (or `[toolchain]` in config) | per-language/glob `formatter` and `linters`, plus one workspace `test` command; after edit/write, the matching formatter and linters run on the touched file. After every otherwise-successful turn, the test command runs once and its failure is appended to durable context. Sugar over the public hook API (dogfooding rule) |
| Themes/keybindings | `themes/*.toml`, `keybindings.toml` | TUI only |

Configured formatter, linter, and test commands can declare
`toolchain.runtime_read_roots`: at most 32 absolute UTF-8 paths, each at most
4096 bytes. Paths must exist and are canonicalized before their executor
generation is published. For a home-installed Rust toolchain, declare the
actual rustup shim directory and runtime directory explicitly:

```toml
[toolchain]
runtime_read_roots = ["/home/alice/.cargo/bin", "/home/alice/.rustup"]
formatter = "rustfmt {file}"
linters = ["cargo clippy --offline --workspace"]
```

These read-only grants apply only to the configured toolchain hooks. General
Bash and declarative shell hooks do not inherit them. They grant no additional
writes or network access; credential exclusions still apply. Linux adds them
to its reviewed system-read baseline. macOS retains its existing general-read
policy with credential exclusions. Project declarations share the toolchain
configuration's explicit project-trust gate; merely setting PATH or HOME grants
nothing. Child and workspace-replacement generations rebuild the same scoped
executor from the captured configuration.

Mode files use one bounded schema. The file stem must match `id`; `permission`
selects the built-in permission floor (`discuss`, `plan`, or `execute`), while an
optional non-empty `allowed-tools` list further narrows the session registry.
An empty list leaves the registry unchanged. Project modes remain inert until
the extension inventory containing them is trusted. The security-sensitive
built-in ids `discuss`, `plan`, and `execute` are reserved: a discovered file
using one of those ids fails registry composition instead of changing the named
built-in's permission contract. Custom ids can select any of the three permission
floors.
When any mode becomes active, its canonical semantic fingerprint (id,
description, permission floor, prompt, and sorted tool allowlist) is persisted
without its source path. Resume and rewind compare that fingerprint against the
trusted registry and fail closed if the definition was removed or changed.

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
  "name": "my-plugin", "version": "1.0.0", "protocol": 3,
  "capabilities": {
    "tools": [ { "name": "...", "description": "...", "schema": {...}, "caps": ["reads-fs"] } ],
    "commands": [ ... ],
    "hooks": [
      { "name": "pre_tool", "class": "policy", "failure_policy": "fail-closed" },
      { "name": "post_tool", "class": "transform", "failure_policy": "fail-open" },
      { "name": "session_start", "class": "policy", "failure_policy": "fail-closed" }
    ],
    "providers": [ {
      "alias-prefix": "custom/",
      "capabilities": ["models"],
      "credential-references": ["providers.custom.api_key"]
    } ],
    "event_subscriptions": [ "tool_call_finished", "turn_finished" ]
  }
}
```

Capabilities are permission-gated: the user approves a plugin's capability set on first load (recorded, re-prompted on change) — a plugin that suddenly declares a `pre_tool` hook after an update is a supply-chain signal, not a silent upgrade.

Provider alias-prefix syntax and its 128-byte wire limit are owned by the extension
protocol. Rust manifest validation and core provider composition use the same
validator, while the TypeScript SDK pins the matching limit through its protocol
fixture and conformance tests; downstream composition must not impose a narrower
private limit after a plugin has been accepted.

### Structured provider output

`ProviderRequest.output` is required. `{ "mode": "text" }` preserves incremental
text, reasoning, and tool workflows without output buffering or a memory claim.
`{ "mode": "json_schema", "name": "result", "schema": ... }` requests one
complete JSON object. Its finite schema uses `string`, `number`, `integer`,
`boolean`, `null`, `array { items }`, `object { fields }`, and `nullable { value }`.
Each field is `{ name, schema }`; every field is required and extra properties
are rejected. Nullable fields must be present with either null or their value.
There are no references, arbitrary keywords, defaults, or coercions. Numbers
must be finite; integer validation inspects decimal significance rather than
rounding a fractional JSON number to an integer.

A schema has at most 256 nodes, depth 16, 64 fields per object, and 64 KiB of
retained schema storage. Field names contain 1–128 UTF-8 bytes; the schema name
contains 1–64 ASCII letters, digits, underscores, or hyphens. The root is an
object. Structured requests require an empty tool list and `tool_choice: {
"mode": "none" }`; incompatible requests fail before discovery, credentials,
or inference. OpenAI Chat uses `response_format.json_schema`; Responses uses
`text.format`, both with strict schemas. Generic compatible Chat, Anthropic,
and Copilot adapters reject this contract before effects. A provider plugin
must explicitly advertise `structured_output: "json_schema"` in each model's catalog;
`"unsupported"` rejects structured requests. The
host accepts structured requests only against already cached model evidence.

The common host validator withholds structured text until normal completion,
valid JSON, and the full schema all agree. Output is at most 256 KiB, 4,096
nodes, and 4,096 normalized events. Duplicate keys, undeclared keys, missing
fields, trailing documents, truncation, refusal, cancellation, and unexpected
tool events never produce a successful terminal. Valid text is delivered in
UTF-8 chunks of at most 16 KiB. Usage and reasoning observations can arrive
before the final object. Raw recording frames remain unchanged; replay applies
the same validator before comparing the normalized recorded outcome.

A process-wide 16 MiB working owner admits at most eight structured responses,
with a conservative 2 MiB claim acquired before adapter effects. That claim
covers the bounded schema and wire projection, 256 KiB accumulation, up to
512 KiB parser scratch, and temporary publication overlap. Validation borrows
JSON tokens rather than allocating a second document graph. The admitted CPU
worker retains schema, accumulated text, and the claim until actual retirement,
including when its waiter disappears. Recorder and router preserve a private
contract fingerprint; they do not create a second live output buffer. Provider
HTTP and RPC transport admission still owns its separate wire bodies.

### Hook catalog

Plugin tools use typed operation admission. `tool/call` requires
`lifetime: { total_ms, idle_ms }`, with `0 < idle_ms <= total_ms <= 300000`.
The host defaults to 300000 ms total and 90000 ms idle. Valid `tool/progress`
notifications identify the request and carry a strictly increasing sequence plus
bounded plain-text progress and optional completed/total work counts. Progress
renews idle time, never total time. Each side coalesces pending observations;
the SDK sends at most four per second, and the host admits a burst of four plus
four per second per operation. Sixty-four admitted requests bound aggregate
progress state. Progress is transient and cannot substitute for a tool result.
SDK completion closes pending progress and awaits its current physical write
before sending the final outcome. Tool timeout, cancellation and caller drop
retain the same native-process settlement barrier as other effectful requests.

Hooks receive a tagged `HookInput` and return a `HookDirective`. `rw-types` owns
these types; the plugin contract generator produces TypeScript declarations and
JSON schemas, and standalone validators check SDK boundaries without runtime AJV.
Every hook declares its class: `transform` or `policy`. The dispatcher orders
classes in that order, then priority, then ID. Policies observe transformed input
and must fail closed. Asynchronous observation uses durable event subscriptions:
commits wake independent bounded cursor readers, whose acknowledgement, replay
and physical settlement are owned by the session. Observation cannot delay a
hook phase or alter its decision.
Pre-permission checks and SessionEnd cleanup remain synchronous policy phases;
they are not asynchronous event subscriptions. Shell and WASM hook components
participate in synchronous phases; asynchronous event handlers use the RPC tier.

A phase admits at most 128 hooks. It has one aggregate execution deadline equal
to its largest declared invocation timeout, with a five-second default and a
ten-minute ceiling. Handler count cannot multiply that budget. Native RPC hooks
use five seconds. First-use native readiness has one absolute thirty-second
activation deadline shared by the generations actually selected during dispatch.
Selection uses the current transformed input; unrelated plugins remain dormant.
Only owned readiness intervals pause the execution clock. Previous callbacks,
directive validation, and dispatch gaps remain charged, and no handler restarts
the execution allowance. Ready generations skip readiness even after its deadline.
Approval verification, immutable snapshot capture, operating-system execution,
authenticated supervisor hello, and protocol initialization share the activation
deadline and cancellation authority. A late hello cannot grant execution. Readiness
cancellation retains its generation owner through the existing five-second
activation settlement proof. Trace fields report readiness, execution, and total
phase latency separately; readiness remains visible in end-to-end latency.

A separate two-second allowance bounds callback settlement. Timeout,
cancellation, panic, and caller drop retain the invocation's admission permit
through effect settlement. A failed or unproven settlement closes admission and
fails the operation regardless of its declared failure policy. Killing a process
without proving its effects settled cannot complete a hook.

Transformations expose only mutable fields. Tool call identity, session identity,
turn identity, and permission capabilities are immutable. A post-tool transform
cannot clear a tool failure. Permission policies fold `allow < ask < deny`; denial
stops dispatch. An `ask` decision requires fresh approval, including when a
remembered approval or ordinary allow rule exists. Hook approval cannot override
an explicit deny rule or mode restriction. Rewritten tool arguments are authorized
again before execution. Workspace-mutating pre-tool hooks run inside the tool's
checkpoint and cannot rewrite the authorized invocation. Workspace-mutating
`turn_end` policies run only after a successful provider turn. The engine authorizes
the exact hook set and its capabilities, captures an opaque workspace checkpoint,
and waits for physical hook effects before finalizing the checkpoint and turn.
An active background shell blocks this mutation phase. Permission denial, policy
failure, or unproven settlement prevents successful turn completion.

| Hook | Mutable fields and policy |
|---|---|
| `session_start` / `session_end` | Observe lifecycle or block; session identity and workspace are immutable |
| `user_prompt_submit` | Transform prompt content or block submission |
| `pre_tool` | Transform tool name/arguments or block execution |
| `post_tool` | Transform typed output and preserve or set its failure flag |
| `pre_compact` | Supply context, summary prompt, and continuation policy |
| `turn_end` | Observe completion or block successful completion |
| `permission_check` | Supply `allow`, `ask`, or `deny` without changing the request |

Plugins can also *push*: `session/inject_message`, `session/set_status`, `ui/notify` — enough to build things like pi's live extensions (todo watchers, budget guards, custom status widgets).

`session/context_read` returns revision-bound pages of at most 128 context item
identities, semantic kinds, provenance classes, token estimates and surgery state.
It exposes no prompt bodies, tool outputs or local paths. A changed revision returns
`restart`; plugins start a fresh inventory read. `session/control` accepts explicit
pin, eviction, mode and model selections. Each method requires its own approved
manifest capability. Session identity is fixed by the host, never a supplied driver
identity. Controls return `busy` during active work or unresolved model selection;
context protection, plan approval and the ordinary model context-transfer question
remain actor-owned. `applied` follows durable commit; a model awaiting a context
choice returns its question identity instead.

### Provider plugins

A plugin can register an inference route (capability `providers`): the router
forwards provider-neutral IR requests for matching aliases over RPC. A provider
can declare the approval-fingerprinted `models` capability and answer
`provider/models` with a bounded catalog containing model capabilities,
cache-breakpoint behavior, context/output limits, and optional pricing. The
`RpcProviderAdapter` exposes that discovery and metadata through the normal live
catalog, model binding, and accounting paths.

Providers can also declare bounded, approval-fingerprinted
`credential-references`. For `provider/http`, the plugin sends the declared
reference, the exact active host provider or catalog request identity, and a
credential-free request. The host matches the exact alias and immutable deadline;
tool, hook, command, unrelated and completed invocations have no provider HTTP
authority. The host resolves and registers the
secret, attaches it to the requested header, and owns the guarded HTTP request;
the raw credential and authenticated request representation never enter a
JSON-RPC value. `provider/http_event` streams a redacted response head and
redacted body chunks (including secrets split across source chunks), while
`provider/http_cancel` propagates cancellation. Requests remain constrained to
the plugin's public `allowed_domains` entries, matching an exact host or
subdomain. An undeclared alias/reference pair is a capability violation and
terminates the plugin (ADR-022).

Provider plugins record and replay normalized provider events through
`WireMode::NormalizedReplay`; plugin-specific wire bytes are outside the
recording contract.

Host callbacks reserve their declared response envelope before starting effects.
A shared 64 MiB allowance covers reply construction, decoded values, and redaction
scratch. Each method uses its source contract for the encoded ceiling, so a tool
can call back into state without reserving a maximum content page at every level.
Typed source owners move into admitted CPU workers for encoding and structural
validation; decoded replies retain their owners through redaction and transfer
to the byte-bounded RPC writer. Redaction reserves each replacement's working
allocation before constructing it. Cancelling a waiter cannot release a worker's
memory or transfer its allowance to an error frame. The complete RPC envelope
must fit the frame limit; oversized replies fail without truncation.

### SDKs

Official plugin SDKs: **TypeScript first** (npm `@rottweiler/plugin`), Rust second. The dependency-leaf `rw-plugin-protocol` crate owns the public wire version, methods, limits, envelopes, manifest grammar, and DTOs. Its checked-in TypeScript, schema, and fixture projections are generated and CI-checked. After the matching GitHub release is published, a separate manually dispatched workflow checks out that exact tag, requires the workspace and SDK versions to match it, and publishes the TypeScript package through npm trusted publishing. It then proves an unmodified clean scaffold can install the same bytes from the public registry. Registry publication does not gate the binary release, Homebrew publication, or signed update metadata. Pull-request CI consumes the packed package artifact rather than rewriting the dependency to workspace source.

### Plugin configuration and approval

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
source = ".agents/plugins/example"
inherit_env = []
allowed_domains = []
```

The TypeScript `source` target owns `manifest.json`, `package.json`, `bun.lock`,
and `src/index.ts` as one package. It cannot be combined with `argv`, `manifest`,
or `cwd`. The separate any-language executable target uses literal `argv` plus
one `manifest`; it is not a fallback for an invalid source target.

Durable tool attachments use the required `ToolCallFinished.payloads` list. Each
session-scoped reference contains a lowercase digest and an exact byte length;
provider conversation IR continues to contain only the compact tool response.
The digest authenticates a versioned manifest containing the length and checksums
of every 64 KiB body chunk. Bounded reads verify the manifest and each fetched
chunk, so modifying both a chunk and its adjacent checksum cannot change a
journal-retained identity. The descriptor-bound session store admits at most
512 immutable objects and 256 MiB of logical body bytes; individual objects are
at most 64 MiB. Quota exhaustion rejects publication rather than evicting a
history-referenced body. Reopen preserves published objects. Fork payload copies
use independent files and bounded buffers. Raw UTF-8 windows and literal line
queries return at most 192 KiB, with explicit byte cursors and an indication when
a matching line itself was clipped. Queries have a 512-byte limit and never
materialize the full body or an intermediate list of matches.

MCP overflow uses this session namespace, rather than a process-global temporary
spool. Namespace creation and inventory occur on first payload access in an
admitted I/O worker; an unused MCP catalog creates no payload directory. The
shared live secret registry redacts the encoded body before its durable identity
is computed. `mcp_overflow_read` requires `reference`, `offset`, and nullable
`query`; it returns `next_offset`, `has_more`, and `line_truncated` with the bounded
window. A fork copies only bodies referenced by its selected canonical prefix.
Slash-command prompt replies are inline-only: an oversized result is rejected
before payload publication. Full tool responses use the canonical attachment path.

Ingress, response catalogs, encoding, redaction and payload windows share a 256 MiB process-wide working-byte
pool with a 128 MiB per-reservation ceiling. CPU and filesystem jobs also acquire
the finite process-wide resource classes. Their actual workers retain the source,
allocation and namespace owners through completion when a caller is dropped;
returned windows and compact results retain their allocation through canonical
tool publication. Publication failures before rename refund quota only after
proving that neither staging nor published bytes remain and syncing the namespace.
An uncertain publication retains its charge until its immutable identity is
verified; it is never treated as an evictable cache entry.


MCP transport framing reserves encoded capacity and parser scratch before reading
or growing a frame. Stdio frames are at most 4 MiB, HTTP JSON bodies 64 MiB, and
SSE events 16 MiB. A structural preflight bounds depth and node counts before
concrete method-specific decoding; it reserves the simultaneous typed and
conversion graphs in addition to raw storage. A frame within the wire ceiling
can still exceed structural or aggregate admission. Such input is rejected
explicitly. Correlated request owners are registered before the SDK queue, and
retain the decoded response through oneshot delivery, catalog/value conversion,
encoding, and canonical tool-result admission. Numeric-string response IDs retain
the protocol's numeric matching behavior; cancellation IDs use exact matching.
Unknown or duplicate response IDs close the connection. The SDK response cache is
disabled, and reviewed catalogs retain their own allocation credit.

Deferred tool, resource, and prompt listings are admitted projections of those
catalogs. An owned CPU worker counts selected entries and field bytes while
borrowing the catalog, reserves destination credit, then copies metadata. It
never copies schemas into a deferred index. The deferred JSON fragment has a
32 KiB write-time ceiling; escaped framing and the resulting system turn retain
their construction owner into immutable session context. Dynamic prompt commands
retain their admitted catalog and descriptor storage with the actual registered
handler. Registry clones share registration and name backing rather than copying
those strings; unregistering one registry cannot retire another one's storage.

Live command-catalog snapshots expose borrowed descriptors from the same immutable
registry generation. They retain the registrations and their metadata source
allowances without copying descriptor strings or exposing handler execution.
Replacing a generation leaves prior readers charged until their final snapshot
retires. Wire catalog projection measures borrowed fields before copying the
accepted bounded prefix.

The product links the rmcp client protocol state machine with owned transports.
The SDK's server and I/O codecs are enabled only by explicit `test-support`
fixture builds and development dependencies for external interoperability tests;
the HTTP test server is likewise a development dependency.

The `rw` MCP server uses an owned stdio dispatcher with the same raw-frame and
working-byte pool. It admits at most 64 outstanding requests before task creation,
rejects duplicate active IDs, and retains each decoded body through bridge work
and physical reply output. Each bridge declares and acquires its construction
allowance before producing a response; result text and structured JSON are bounded
before duplication. Wrapper `rottweiler_tools_call` requires both `name` and
`arguments`. A connection owns at most 32 explicit or newly created session IDs.
Cancellation suppresses only that request's reply. Deadlines can report failure
while accepted engine work remains owned; disconnect joins that work before
returning. A wakeable stdio worker preserves pipe and regular-file input, restores
descriptor flags, and joins before refunding its fixed 2 MiB stack. The shipped
server exposes stdio; no unowned HTTP `ServerHandler` adapter is exported.

Unadvertised host requests are answered under their decoded owner, without
spawning detached payload handlers. Catalog-change notifications invalidate one
shared generation bit. A remote cancellation settles its exact pending request;
its reason is never reflected into another request. Initialization metadata stays
owned through every private peer holder and the completed service/request joins.
Connection creation retains pending parse and I/O workers even if its caller exits.

Each HTTP connection owns one private current-thread runtime, with at most one
resolver thread and 64 active exchanges. The two 2 MiB stacks and bounded runtime
metadata are admitted from the same MCP pool before startup; a separate 64-worker
limit bounds service count. No finite Blocking permit surrounds the persistent
runtime, so resolver admission cannot deadlock its parent. Actual native thread
join precedes stack refund; an unproven join quarantines its physical owner.
Request bytes carry their allocation inside the actual network body. Each exchange
reserves the bounded network chunk overlap before invoking the injected client.
Cancellation of an exchange retires the connection and fails its pending calls.
GET network failures drain active exchanges and destroy the failed runtime epoch
before reconnecting with the same protocol session and retained resume cursor.
Graceful close sends session DELETE before runtime teardown. No cleanup path
constructs an ambient client or abandons DNS work on a timeout.

HTTP preserves accepted asynchronous responses, JSON-RPC errors on non-success
HTTP status, SSE resume/retry semantics, and negotiated protocol/session headers.
Only minimal validated tool-header annotations are retained alongside the reviewed
catalog. Outgoing headers are bounded to 32 entries and 32 KiB; incoming metadata
retains the guarded transport's 128-entry/64 KiB bound. Individual values are at
most 8 KiB. Namespace/session identifiers have their separate protocol limits.


`/mcp` shows connection and approval state. Stdio servers receive only intrinsic runtime reads, scratch writes, and no network by default. `read_roots`, `write_roots`, and `allowed_domains` are bounded explicit process grants; roots must stay within active workspace authority, and domains use the supervised policy proxy with DNS pinning and private/local-address denial. Separately, virtual MCP tool calls classify as `network + exec` unless user-level `capability_overrides` supplies a server default or an exact per-tool override (`reads_fs`, `writes_fs`, `network`, `exec`); project configuration cannot downgrade this permission classification. Tool entries take precedence over the server default. Approval is bound to both kinds of grants together with the exact origin, transport, argv/environment names, OAuth references, and configuration fingerprint; changed configuration requires a new explicit fingerprint confirmation. `rw mcp login <server>` uses Authorization Code + PKCE and atomically stores the access token, optional refresh token, expiry, and exact resource/audience binding in the Rottweiler credential vault. Expired access is refreshed only against the same trusted token endpoint/client/proxy configuration, and a rotated refresh token is durably replaced before the new bearer is exposed. Plaintext tokens and environment-backed MCP OAuth references are rejected. Remote prompts are available through `/mcp.prompt <server> <prompt> [JSON object]`; catalog-derived namespaced aliases are conveniences and the stable command resolves the live server state at invocation.

Stdio activation copies the exact approved executable and attested interpreter inputs into private snapshots before starting the server. The physical process retains those snapshots through process and proxy settlement. The captured command also binds literal arguments, resolved environment, working directory, and sandbox grants; a different launch request cannot reuse it. Executables must support relocation without changing their approved bytes. macOS system executables with location-constrained signatures are unsupported by this snapshot path; launch refusal does not fall back to the installation path. Stdio keeps its declared workspace working directory and filesystem grants while file arguments point to their attested snapshots.

The MCP manager admits at most 64 registered servers and 64 concurrent invocations.
A connection generation owns initialization, catalog discovery, and retirement;
concurrent enable requests share that transition. Invocation owners include response
encoding and overflow writes. Cancellation and deadlines never drop those futures.
The tool settlement barrier waits for abandoned effects, and reports unproven
retirement as `EffectsUnsettled`. Disable drains the exact connection and its invoked
work before completing. Shutdown closes admission permanently and retains its cleanup
when a caller stops waiting. Changed catalog schemas remain inactive until approval.


`rw plugin status|approve|revoke` manages the separately fingerprinted plugin approval ledger. `rw plugin check <path> --allow-exec` validates source-package identity and runs the declared typecheck and test scripts without attaching the plugin to a session. For a source target, the release-owned sibling host discovers the complete graph without executing top-level plugin code; Rust validates the Bun lock identities, copies exact no-follow bytes to private scratch, rebuilds from that sealed tree, requires the second graph to match, and publishes one content-addressed bundle. Approval binds the manifest, source graph, lockfile, bundle, host ABI, format, origin, environment names, domains, and sandbox policy. Each plugin launches in its own host process. A source failure leaves other plugins running.

The scaffold contains one inert `manifest.json`, imported through `parsePluginManifest`, so authority has one owner. `rw plugin dev <path> --session current --allow-dev-exec` attaches that package to one live local session. Capability changes require detach and an explicit grant; development approval is never written to production approval.

One native generation owns configured and development plugins, provider routes,
tools, hooks, commands, UI identities, and event workers. Replacement closes child
admission before proving old plugin and delivery effects settled. It rebuilds
first-party MCP, orchestration, and workflow registrations from the same root and
extension recipe. A retained child generation prevents replacement.

Preparation runs in an independently owned command task so the actor can service
retiring plugins' host callbacks. The actor commits a root change, installs its
complete candidate, then synchronously publishes the matching native gate, model
routes, and delivery workers. Candidate workers cannot consume events before this
publication. A dropped requester does not abandon preparation. Lost authority or
failed proof after retirement leaves admission closed; no stale adapter can
silently resume. Session state and delivery acknowledgements remain canonical
across process replacement, while live UI ownership receives a fresh identity.

Native execution is code-only: runtime/code reads and private scratch writes are
available, while workspace access, networking, child processes, and application
launch delegation are denied. Tool handlers request filesystem or HTTP work through
an invocation-correlated host scope. Approval and checkpoint coverage come from the
outer engine tool call; sibling tools, hooks, and providers do not expand that scope.
Nested process, orchestration, MCP, and interactive operations are denied.

The separate executable target pins its executable, explicit interpreter entrypoints, adjacent dependency descriptors, manifest, code root, origin, environment names, and domains by canonical path, length, and SHA256 identity. The primary executable owns one identity; the additional attested-file set contains only other code and dependency files. Together they are limited to 64 files and 256 MiB. Repeating the executable in discovered inputs cannot replace its original approval identity or cause a second attestation read. At activation the executable is copied into the shared approved executable owner, and attested code files into a private read-only hierarchy. File arguments and the working directory refer to that hierarchy; unlisted package files do not receive an ambient read grant. The physical process retains executable and code snapshots through proven process and proxy settlement, including cancellation or failed handoff. Eval, module-runner, package-runner, and `PATH`-resolved forms are rejected.

On macOS, executable snapshots use a filesystem copy-on-write clone from the already opened source descriptor into an atomically private directory. The host hashes the cloned snapshot against the approved length and digest, then rechecks the source descriptor's identity, size, mode and modification/change timestamps before publication. Unsupported or cross-filesystem clone operations use the same verified streamed copy; other failures refuse activation. Both paths retain the private descriptor and directory through physical process settlement, and charge the complete logical image size to the existing image allowance. Snapshot construction does not execute or prewarm extension code.

The Rust host and TypeScript SDK consume the same generated contract projections. Provider plugins emit request-correlated `provider/event` notifications within host-issued event/byte credit windows. Four streams share a bounded process budget; each has a fixed five-minute total deadline and reserved terminal storage. Dropping a consumer initiates whole-process teardown, and the invoked provider remains owned until local effects settle. Control and response traffic has a separate bounded queue with priority between data writes. Catalog and host-HTTP requests are separately bounded and negotiated. `packages/plugin-sdk/PROTOCOL.md` explains the wire contract; its schema and fixture are projections of `rw-plugin-protocol`, not additional owners.

The public documentation site in `packages/docs-site` copies the protocol
crate's generated schema and fixture byte-for-byte and links to the SDK's
protocol Markdown. It adds searchable navigation, raw Markdown, and direct
artifact downloads without recreating protocol values. CI checks both the
protocol projections and the site.

## Tier 3 — WASM hook components

Private Wasmtime component-model helper for hook extensions (ADR-021). It is bundled beside `rw` but is not a second app or public command: `rw` starts it only to validate or invoke an enabled extension. A component exports this versioned WIT surface:

```wit
package rottweiler:extension@1.0.0;

world hook-extension {
  export invoke: func(event: string, payload-json: string) -> string;
}
```

The return value is a bounded `HookDirective`: `continue`, a phase-specific `transform`, `permission`, or `block`. The component declares hooks through the same `PluginManifest` used by Tier 2 and is registered on the same `HookDispatcher`; ordering, deadlines, cancellation, and fail-open/fail-closed behavior therefore do not fork.

The bundled helper has a strict `rottweiler-wasm-host.identity.json` receipt containing its byte count and SHA-256 digest. The host verifies and snapshots those exact bytes before admitting a worker generation. Each worker retains its executable authority through actual process settlement; replacing an installation path cannot change an admitted generation.

The public `rw` binary does not link Wasmtime. It communicates with the private helper through an application-owned bounded worker pool and a typed, length-prefixed load/call protocol. A cache miss transfers the exact signature-verified component bytes; warm calls reuse compiled code; malformed or oversized frames fail closed. There are no host imports and no WASI. Every call uses a fresh store with bounded component and serialized-input bytes, per-memory size, memory/table/instance counts, and fuel. Queue wait, loading, and invocation share one fixed deadline. Cancelled, timed-out, malformed, or trapped workers are retired; their slots remain charged until the process is reaped. Each factory shares at most two workers across sessions and admits at most 32 requests. Each worker retains one compiled generation. Explicit factory shutdown closes admission and drains workers. Fuel periodically yields from the async Wasmtime call so dispatcher cancellation stops guest execution rather than merely abandoning a blocking worker. Enabled components are also bounded by count and aggregate installed bytes. The owned-string result is checked immediately after canonical lifting; its unavoidable pre-check allocation remains bounded by the store's linear-memory ceiling. The component interface supports hooks. Tools, commands, providers, event subscriptions, and pushes use RPC plugins with streaming and permission contracts.

### Signed extension registry

Registry catalogs are bounded refreshable caches, and every entry is validated before it can be printed. Every release binds the exact name, semantic version, capability manifest, HTTPS artifact URL, byte length, lowercase BLAKE3 digest, and publisher public key with an Ed25519 signature. The catalog cannot nominate its own trust: installation succeeds only when the publisher key is supplied independently and pinned in the separate `trusted-publishers.json` approval catalog, and component bytes must match the signed size and digest. A staged install atomically publishes the artifact and signed release record, but never embeds its own trust anchor. Enabling re-verifies the release against the separate publisher pin before displaying capabilities and records lowercase publisher-key, manifest, and component fingerprints in `enabled.json`. Activation names use the same bounded canonical grammar as manifests. Installed files are opened with no-follow descriptor traversal on Unix and bounded opened-handle checks elsewhere. A broken enabled extension is skipped with a control-stripped durable warning so `rw extension disable` remains available for recovery. Session composition registers signature-verified, manifest-checked proxies without starting workers or compiling components. The first matching hook invocation owns compilation within its deadline and applies its declared failure policy. Workspace-root changes reuse the session's captured hook generation; they neither reread mutable extension files nor ignore registration conflicts.

`rw extension registry list --catalog <https-url>` fetches a bounded catalog through the hardened signed-update network path. `rw extension registry install <name> --catalog <https-url> --publisher-key <base64>` selects the latest semantic version (or an explicit `--version`), verifies the independently supplied publisher key, signature, URL, size, and digest, then installs it inactive. `rw extension enable <name> <version>` safely reloads the signed local record, displays and confirms the exact manifest, asks the bundled helper to compile the component, and pins its manifest/component fingerprints. `status` shows inactive, enabled, missing, and tampered records explicitly; `disable` removes only activation. A changed installed file is rejected and skipped on the next session start, with a durable warning visible to attached clients.

## Extension of the extension system

- `rw plugin scaffold --lang ts` generates a working plugin skeleton with tests.
- `rw plugin dev <path> --session current --allow-dev-exec` hot-reloads a source plugin inside a live local session.
- Protocol 3 is the sole accepted generation; upgrades migrate every caller and
  remove the previous contract in the same change.

## What extensions can never do

These are permanent security invariants, not gaps awaiting work. Any future
protocol version must preserve them.

- Read raw credentials/API keys from the engine.
- Bypass the permission engine or sandbox (a plugin tool's `caps` manifest is enforced, not trusted).
- See redacted secrets (redaction runs before events reach plugins).

## What extensions cannot do yet

These are known gaps in the current protocol, not deliberate restrictions. They
are expected to close without weakening any invariant above.

- Preserve arbitrary provider-specific wire frames in recordings. RPC provider
  plugins replay their normalized event stream; wire-fidelity replay through a
  plugin needs a larger replay-through-plugin protocol.
- Add a new core `AdapterKind` or `WireMode` through configuration. Those enums
  remain closed correctness boundaries (ADR-024); a novel dialect belongs in a
  versioned RPC provider.
- Run a provider in the WASM component tier. Its current ABI accepts hooks only;
  provider streaming, cancellation, host-mediated authentication, and
  recordable framing remain on the trusted native RPC tier (ADR-025).

### Durable subscription ownership

Extension event delivery uses the closed `ExtensionEventKind` catalog and a
host-bound namespace. One worker per plugin reads canonical journal pages from
its acknowledged cursor. The actor only coalesces wakeups after durable commit.
The callback returns state mutations; the actor commits them with the exact
source acknowledgement using compare-and-swap revision authority. Failed
callbacks or revision conflicts leave that cursor unacknowledged. Recovery may
repeat external effects, so handlers must make those effects idempotent.

The application plugin budget admits at most 64 delivery workers. Preparation
is serialized; retained event sources and projected requests share 64MiB.
Source bytes remain charged through the last reader. Callback and host-command
traffic use separate bounded lanes, and source admission is revoked when the
callback settles. Shared journal decoding owns its own input admission; the
encoded line limit is not a claim about arbitrary JSON heap amplification.

### Driver-scoped navigation

An active extension command can request `session.control({ action: "navigate", target })`. A target is either `{ kind: "session", session_id }` or `{ kind: "transcript", sequence }`. The host validates the target, retains at most one navigation request with the command, and emits `SessionNavigationRequested` only after successful handler settlement under the same driver and runtime generation. Background pushes cannot navigate. The built-in `/goto session <id>` and `/goto sequence <number>` commands use the same control owner.

Navigation is a connection-scoped request to the initiating client. It grants no authority over the destination session and is neither journaled nor replayed. Clients use their ordinary session open and bounded transcript read paths. Transcript navigation rejects future sequences; a discarded source resolves to the nearest surviving row at or before the requested sequence and exposes that replacement to the client.

### Invocation-bound host tools

A command declares exact host tool names in `allowed_tools` and requests them
through `session/tool_call` with its host-minted invocation identity. The host
binds that request to the currently admitted command, its driver, mode and
runtime generation. A missing, foreign or retired origin has no tool authority.
One command may have one outstanding host tool call. The actor continues serving
status, state and interaction responses while the tool runs.

Host tools use the session's ordinary mode restrictions, permission gates,
pre/post hooks, mutation checkpoints and effect-settlement barrier. They create
canonical tool-only turns with host invocation identities; they do not fabricate
provider messages. The callback receives its outcome after canonical tool and
turn completion. Output exceeding the callback byte limit is explicitly absent;
its complete output remains in the canonical tool event.

Authenticated HTTP operations share an application-wide admission limit of eight.
Each retains a private network runtime, a supervised egress proxy and an eight-frame
response channel (256 KiB per frame). Cancellation drops the response path inside
that owned runtime. Completion waits for its connection and blocking resolver tasks
and all proxy workers to settle. The terminal HTTP outcome follows this proof;
stream data alone cannot complete the provider call. A failed or five-second
expired cleanup proof closes plugin admission and reports `effects_unsettled`.
The admitted owner continues observing slow cleanup and returns shared capacity
only after actual proof; an unprovable or panicked owner stays quarantined.
This proves local resource retirement;
it does not assert that a remote service stopped inference or settled billing.

### Session status

A plugin status is a short status-bar value: at most 1,024 UTF-8 bytes, without
control characters. An empty value clears that plugin's entry. Each session
admits at most 64 nonempty plugin statuses; updates to an existing entry and
clears remain available at capacity. Admission happens before journal append.
Rich text and larger content belong in UI panels. The session-state snapshot
returns each retained status with its canonical source sequence so clients can
recover it without replaying the session transcript.

A child actor captures its own native plugin endpoints, state namespace, UI registry,
commands, hooks and event delivery. Provider configuration is inherited as an inert
recipe; the child supplies its own endpoints when constructing its lazy model.
Its workspace roots remain fixed until close and rebind. Parent generation replacement
requires all captured child leases retired. A dropped child construction or close
caller cannot release that lease before the child's native cleanup owner settles.

A native plugin helper must authenticate its private supervisor rendezvous before
receiving permission to start the plugin. A rejected handshake retains the helper
through actual reaping. Diagnostics distinguish its pre-kill exit observation
from final wait status and include only complete stderr up to 4 KiB; incomplete
or oversized stderr is omitted. The mandatory host redactor sanitizes these
launch errors before reporting or tracing them.
