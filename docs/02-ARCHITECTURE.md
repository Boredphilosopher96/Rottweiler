# 02 — Architecture

The engine, clients, and extension hosts have explicit owners for state, effects, and resource limits. [Extensibility](04-EXTENSIBILITY.md) defines the plugin and provider contracts.

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
│                rw-runtime                   │
│ session factory · storage/provider/tool     │
│ composition · reusable headless host        │
├─────────────────────────────────────────────┤
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
├── .bun-version               # exact Bun toolchain owner
├── rust-toolchain.toml        # exact Rust toolchain owner
├── Cargo.toml                 # workspace
├── architecture/
│   └── ownership.toml         # checked owners, generators, and forbidden shadows
├── contracts/
│   └── release-contract.json  # release platforms, archive shape, and product budgets
├── crates/
│   ├── rw-operation-contract/ # leaf lifetime/progress values shared across tool and wire boundaries
│   ├── rw-memory-derive/      # compile-time retained-allocation accounting
│   ├── rw-macos-bootstrap/    # Mach authority clearing at worker entry
│   ├── rw-types/              # shared types: message IR, events, config schema, errors
│   ├── rw-plugin-protocol/    # plugin envelopes, manifest capabilities, and shared hook contract
│   ├── rw-store/              # session persistence, checkpoints, config loading
│   ├── rw-providers/          # router, adapters, pricing, auth
│   ├── rw-context/            # token budget, compaction, TOON, cache strategy
│   ├── rw-tools/              # built-in tools + tool registry
│   ├── rw-intel/              # code intelligence: tree-sitter symbol index + LSP client (ADR-016)
│   ├── rw-sandbox/            # OS sandbox profiles + policy classification
│   ├── rw-mcp/                # MCP client/server (rmcp), deferred loading
│   ├── rw-ext/                # extension host: RPC plugins, hooks, commands/skills/agents loaders
│   ├── rw-core/               # the engine: session loop, modes, orchestration, permissions
│   ├── rw-runtime/            # reusable session/host composition for headless frontends
│   ├── rw-wasm-host/          # private capability-bounded WASM runtime helper
│   ├── rw-cli/                # `rw` binary: args, presentation, transports, supervision
│   └── xtask/                 # code generation and release signing tools
├── packages/
│   ├── js-host/               # Private Bun executable with lazy tui/source-plugin roles
│   └── tui/                   # OpenTUI frontend source (TypeScript)
├── protocol/                  # GENERATED (ADR-013): JSON Schema + TS types emitted from rw-types
│                              #   (schemars/typeshare); committed, CI-checked for drift
├── docs/
└── tests/                     # cross-crate integration + replay fixtures + protocol contract tests
```

`rw-runtime::session::compose_local_session` returns an owned `LocalSession` with
its command/event handle and a validated prompt inspection result when requested.
The CLI owns bounded line input, approval input, text/JSON rendering, standard streams, and
performance markers. Runtime composition does not depend on terminal libraries or
write to standard streams; diagnostics go to the client's tracing subscriber.
Closing a local session waits for actor effects, finalizes durable projections and
session-local state, then settles services. Dropping a client or a cleanup waiter
requests that same independently owned shutdown; it cannot cancel cleanup.

The headless REPL accepts UTF-8 lines up to 128 KiB, including a final partial line
at EOF. CR, LF and CRLF delimit lines. Its terminal input supports erase, Ctrl-U,
Ctrl-C and Ctrl-D; rich editing, history and undo belong to OpenTUI. Noncanonical
terminal mode prevents silent kernel line truncation. One polling worker owns
and restores terminal mode and descriptor flags. Fixed line/echo buffers and
nonblocking count/byte admission bound queued input; exhaustion explicitly refuses
the unsubmitted backlog and terminates the REPL. Output is ordered, capped at
64 MiB per retained message, and fails after five seconds without write progress.
Ctrl-C reaches the actor through an independent coalesced signal even during a
blocked write. Shutdown wakes polling, retires queued output, discards unsubmitted
terminal bytes and waits for physical I/O settlement before returning the terminal.

Dependency rule: arrows point downward only. No Rust crate depends on anything in `packages/`. `rw-operation-contract` and `rw-resources` are dependency leaves. `rw-resources` owns process-wide physical execution admission, with no dependency on sessions, tools, or presentation. `rw-types` consumes the operation contract and the allocation derive macro. `rw-plugin-protocol` consumes the operation and shared-type contracts. `rw-sandbox` owns policy and calls the dependency-free macOS bootstrap crate for Mach authority clearing. `xtask` consumes the type, provider, plugin, operation, and storage owners to generate schemas and SDK projections; product crates never depend on it. `rw-core` is independent of `rw-runtime` and all executable frontends. `rw-runtime` owns concrete storage/provider/tool/MCP/extension assembly and injects it into the core engine. `rw-cli` consumes that owned composition API; its direct lower-level dependencies are explicit, narrow administrative and transport commands, not a re-export facade. The metadata and source-layout rules are enforced in CI by `scripts/check-dependency-direction.py`.

Physical execution has independent process-wide pools: 64 supervised process
groups, 64 network operations, 16 blocking workers, and at most four CPU workers
(capped by available parallelism). Each class admits at most 64 waiting requests, with a 30-second queue deadline.
Shell commands reserve both their execution group and independent watchdog group
atomically before either process starts.
The actual worker or process owner retains its lease through settlement; cached
results retain allocation credit separately. Cancellation removes a waiter, not
an already running effect. Nested work transfers ownership or uses a different
class instead of reacquiring its own exhausted pool. A process-group credit
counts one supervised group, not arbitrary descendants within it. These limits
complement per-session queues and permission policy; they do not grant authority.
Finite filesystem/database work and system DNS lookup enter the blocking pool
at their async execution boundary; an HTTP caller disappearing does not release
the OS resolver worker's capacity. Context assembly, event encoding, and symbol
queries enter the CPU pool. Git owns one admitted process group and its
nonblocking pipes in the same finite worker. Cancellation settles that group
before the effect caller resumes; no output-reader task survives independently.
PTY execution keeps its process and IO ownership through retirement, and an
unproven exit keeps the session's shell exclusion active. Cleanup joins and persistent service pumps remain separately owned: they
must keep progressing when ordinary execution admission is exhausted. Failed
settlement retains the physical owner and its capacity rather than admitting
replacement work over effects whose termination is unknown.

Each piece of contract data and each feature catalog has one hand-maintained
owner. Other crates, clients, scripts, tests, and docs either consume that owner
or use a generated projection. Boundary-specific validation remains local, but
it imports the contract that it enforces instead of copying limits or defaults.
`architecture/ownership.toml` records the ownership boundaries that CI can check
mechanically. `scripts/check-ownership.py` rejects duplicate owner locations,
unmarked generated projections, and named hand-maintained shadows. The manifest
is a checked set of high-risk boundaries, not proof that an unregistered semantic
rule has no second implementation.

The exact Rust version lives in `rust-toolchain.toml`; the exact Bun version
lives in the root `.bun-version`. Workflows, package metadata, WSL provisioning,
and build docs project those values. `scripts/check-toolchain-ownership.py`
rejects drift without storing a third copy in the ownership manifest.

The stable boundary is semantic rather than binary-private: `rw-core` owns
protocol state, actor behavior, permission enforcement, and provider-neutral
engine traits; `rw-runtime` owns the reusable `RuntimeSessionFactory` and
`HeadlessRuntimeBuilder`; `rw-cli` owns Clap parsing, terminal/JSON rendering,
socket transport, process launch, and supervision. Runtime implementation
modules never re-export lower crates wholesale, so every dependency remains
visible in the crate that actually uses it.

### MCP connection authority

`McpInboundRouter` owns the inbound request matrix and handshake for both stdio
and guarded HTTP connections. It advertises no server-initiated host capabilities.
An inbound request cannot acquire session, credential, filesystem, or model
access through a library default handler.

| MCP operation | Contract |
|---|---|
| Tools, resources, prompts | List only server-advertised capabilities; use bounded reviewed catalogs and owned unary calls |
| Ping | Reply without session authority |
| Sampling, roots, form/URL elicitation, task/custom requests | Not advertised; reject with `METHOD_NOT_FOUND`, without selecting an answer or reflecting payloads |
| Catalog/resource change notifications | Revoke the connection's catalog snapshot; hide its definitions and reject new calls until explicit reconnection and schema review |
| Cancellation | RPC request owner handles the cancellation token; physical operation settlement remains independently owned |
| Progress, logging, task/subscription/custom observations | No subscription or authority is granted; discard payloads without a retained queue or user-visible secret channel |

Catalog invalidation uses one shared atomic flag per connection. Disconnection
also revokes the snapshot. Reconnection waits for prior invocation ownership and
settles the exact prior client before opening a replacement; changed tool schemas
remain inactive until approved. Adding an inbound host capability requires a
session-authorized command route, a declared handshake capability, and explicit
cancellation/settlement behavior in this owner. Terminal and embedded clients
receive the same unsupported-capability behavior.

### Process model

`rw` (Rust) is the single entry point. In TUI mode it: binds the engine server to a unix socket (localhost TCP on Windows) with a per-engine auth token, spawns `rottweiler-js-host tui` with the socket address + token, and supervises it. Engine and TUI fail independently: TUI crash → `rw` restarts it and reattaches to the live session; engine crash → TUI shows a reconnect state, sessions recover from the event log. Headless startup runs only Rust. A source-plugin invocation lazily starts `rottweiler-js-host source-plugin`; this role never initializes OpenTUI, terminal I/O, or parser assets.

The process boundary is not a product boundary. Every supported release is one
platform application bundle containing `rw`, one `rottweiler-js-host` executable with explicit
`tui` and `source-plugin` roles, the private
`rottweiler-wasm-host`, and exactly one native OpenTUI library. Homebrew's
versioned Cask stages the exact macOS archive in its managed directory and
exposes only an `rw` symlink; the versioned Formula keeps
the same files together under `libexec`. The standalone bootstrap downloads
the identical signed archive and its installer exposes only `rw`. Consequently
ordinary install, launch, and close are each one user action even though crash
isolation remains process-based. Source builds may expose component paths to
contributors, but are not an end-user distribution contract.

`contracts/release-contract.json` owns the supported platform mapping, archive
member paths and modes, extraction caps, and product size budgets.
`scripts/release_contract.py` validates that file and generates
`crates/rw-types/src/generated/release_contract.rs` for Rust consumers. Release
shell and Python code query the JSON owner through that script. They do not keep
parallel platform or archive tables.

The signed updater follows the same boundary: `rw-core` owns exact-byte threshold verification, root rotation, rollback/freeze policy, and the opaque proxy-aware HTTP client; `rw-providers` remains the only production HTTP implementation; `rw-cli` owns the official versioned install layout, bounded archive extraction, fsync/journal recovery, and atomic generation selection. Runtime/project config cannot replace the compile-time trust root or metadata origin.

**Remote mode** (ADR-015): `rw --remote <host>` SSHes to the host, starts/attaches an engine there, forwards its socket locally, and runs the local TUI against it — same code path as local, which is the point. Two hard rules this imposes everywhere: no protocol message may assume a shared filesystem with the client (file previews/diffs travel in-band), and reconnect/resync is a tested first-class flow, not an error path.

**Resync semantics**: events carry per-session monotonic sequence IDs. The persisted journal supplies bounded gap pages when a client falls behind the live ring. A TUI attachment first reads source-fenced snapshots of the active transcript tail, session state, controls, children, and tasks, then subscribes from their minimum source cut. Each reducer retains its own cut so replay cannot overwrite a newer snapshot. A `replay_cursor_ahead` response triggers another owned bootstrap. Historical transcript navigation reads canonical pages independently of live catch-up.

HTTP command clients declare the source-owned normal or urgent command lane. The
transport validates the declaration against the decoded command, reserves input
bytes before collection, and shape-checks JSON before typed allocation. Ordinary
input admits the combined per-client read and control window, so simultaneous
legal reads cannot prevent a control from reaching semantic admission. Decoder
slots return before semantic dispatch; retained command bytes remain charged
until dispatch relinquishes them. Normal input has a 96 MiB pool and urgent input a separate 4 MiB pool; body/header deadlines
and a connection cap bound incomplete requests. Runtime client identities and
capabilities are authenticated together with a runtime-scoped key, without retaining
a registration for every connection ever opened.

Host event fanout shares encoded JSON bytes through every intermediate queue and
the final SSE frame. A 96 MiB host-wide owner covers prepared copies, encoding
scratch, and retained output; four encoders and 64 subscriptions (four per client)
bound concurrent work. Final byte clones retain both allocation and subscription
credits. Exhaustion closes delivery so durable events recover from their source;
completed controls remain available through their operation receipts. Session
journal decode ownership is separate from transport encoding ownership.

Host control admission counts prepared typed allocation capacity before hashing or
spawning work: ordinary controls have 64 global slots, eight per client, eight
per session, and 32 MiB of retained command bytes. A single command may retain
at most 16 MiB. Interruptions, cancellation, approvals, and shutdown use an
independent eight-slot, 1 MiB lane with two slots per client/session and 64 KiB
per command. Exhaustion returns an explicit busy response without accepting work.
Accepted controls remain owned after transport cancellation. Shutdown joins their
effect proofs, and unwinding releases completion waiters while poisoning closure.

Control completion reserves bytes before execution. Prepared results, cache entries,
and waiting callers share one allocation lease: 64 MiB for ordinary results and
1 MiB for urgent results. Cache eviction frees only its own reference. A completion
that exceeds its reservation returns `control_result_limit`; its effects have
settled, so the caller must inspect session state before further actions.

Mutation request IDs identify operations across authenticated connections and host
restarts. Clients generate a fresh globally unique request ID for each operation
and retain it for correlated retries. The host fingerprints command content without
the connection's client ID, authorizes the workspace on receipt access, and commits
admission before dispatch. It commits the correlated outcome before delivery.
Completed receipts survive memory-cache eviction; replay rebinds connection metadata
and preserves durable source identity. The private SQLite receipt authority is
separate from the session journal, so it cannot alter transcript action preconditions.

A crash between admission and proven completion returns `operation_indeterminate`;
that receipt never authorizes automatic reexecution. Completed outcomes, including
rejections, are immutable. Session creation, conversation/configuration mutations,
approvals, rewind/review, export, and child continuation use this durable policy.
Read queries run again; connection attachment, interruption, authentication, and
development-plugin attachment have connection/session lifetimes. Fork uses its
explicit operation ID and specialized storage-recovery journal. These classes are
exhaustively defined in the host command policy.

Interrupt admission uses the committed driver lease and exact active turn under
one short control lock. Cancellation and its connection acknowledgement do not
wait for journal I/O or the actor command queue. The actor publishes lease changes
only after commit, and turn completion still waits for effect and journal settlement.
An admitted interrupt cannot cancel a subsequent turn. Actor closure disables this
control boundary and cancels the active turn before awaiting cleanup.

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

IR shape constraint (ADR-013): all protocol-crossing enums use **struct variants with named fields** and typed payloads (no tuple variants, no bare `Value` where a typed shape is known). The sketches above are illustrative; the protocol codegen contract validates the real `Block` and `ToolOutput` shapes and rejects drift.

### Engine protocol (`rw-types`)

```rust
enum ClientCommand {
    SendMessage { session, content, attachments },
    Interrupt { session }, ApproveTool { id, invocation_id, decision },
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

Live model/provider discovery is the narrow display exception to provider-blind
execution (ADR-019). The engine may project bounded concrete model ids and
display names, capabilities, alias membership, availability/current selection,
and provider rows containing only a display name, sanitized auth interaction
kind, boolean auth/reachability state, and model count. This lets `/models`,
`/providers`, and quick-connect flows work without teaching the TUI provider
wire formats. Provider configuration, configured inference/token/discovery
endpoints, proxy settings, credential references or values, account identifiers,
raw transport errors, and route implementation details remain inside Rust.
During an explicit login only, a bounded authorization/verification URL,
loopback redirect URI, and one-time device user code may cross as an ephemeral
connection-scoped challenge because the user must act on them. Challenges are
never durable events: session/control logs, provider recordings, replay,
exports, and reconnect gaps must not retain them. Status strings crossing the
protocol are category-only and size-bounded.

Engine events share one delivery channel, but have two explicit scopes.
Events carrying `CommandAckMeta` are connection-scoped control/query replies:
they have a request id, no session sequence, and may identify a session. This
includes the immediate `CommandAcknowledged` reply and bounded projections such
as mode, model, settings, review, and workspace results. They are never written
to a session log or replayed after reconnect. Events carrying `EventMeta` are
session-scoped, carry a per-session monotonic sequence id, are persisted in that
session's event log, participate in reconnect/resync, and are available to
hooks/extensions. The checked-in durable-envelope schema mechanically excludes
every generated `CommandAckMeta` variant. One event schema, three consumers:
UI, storage, extensions.
The core live ring has at most 1,024 slots per actor, with process-wide 128 MiB
allocation admission for payloads, ring storage, and client identities. At most
512 subscriptions exist across the process, including at most 64 per actor.
Committed batches move their prepared allocation into shared immutable delivery
guards; commit queue credits are released independently. When live bytes fill,
durable publication stores only sequence fences. Replay uses a separate aggregate
allowance for four worst-case 16 MiB source + 64 MiB decoded + 64 MiB preparation
windows (plus 16 KiB descriptors each), shrinking each completed page to its measured
prepared allocation. Pages contain at most 256 events and 16 MiB encoded source.
Replay admission has 64 waiters and a 30-second deadline; dropping a receive future
cancels admission or preserves its already-owned read task for the next poll.
Returned guards retain allocation credit even after ring/page eviction. A consumer
holding all credits receives an explicit saturation failure after the bounded wait.
The actor never waits for subscriber capacity. Evicting an unobserved connection
reply marks only its target subscriptions failed under the publication lock;
a later payload drop cannot hide that loss. Actor close wakes subscriptions, which
finish catch-up through the final durable tail before reporting closure.
Messages must match the generated schema; 64-bit counters and sequence
ids cross JSON as decimal strings so JavaScript clients never lose precision.

Historical `rw stats` is deliberately outside the live engine/provider path. It
copies the reconciled accounting database plus committed WAL through the same
descriptor-stable, size-capped read-only snapshot boundary,
then scans authoritative event logs under aggregate session/byte/event limits
for tool-use counts and durable parent→child relationships. A session-scoped
query includes that session's descendants; descendant accounting is relabeled
as `subagent` instead of duplicating the aggregate `SubagentFinished` result.
USD API subtotal completeness and typed subscription/credit/unavailable counts
remain explicit in both deterministic text and JSON output.

`rw doctor` is an administrative, non-mutating composition root rather than an
engine session. Local config/path, OS/WSL, terminal, and sandbox probes always
run; outbound provider probes require `--network` and enforce a 250–10,000 ms
timeout with redirects disabled. Provider and proxy URLs retain the same
user-scoped precedence as runtime calls. All configured credential references
are inventoried through one process-cached vault manager, after which only
presence/source categories enter the report; versioned ChatGPT/Copilot bundles
are shape-validated behind the core secret boundary. The stable JSON schema
contains no secret-bearing type, and failed checks set a non-zero CLI status.

### Session loop (`rw-core`)

Single-writer actor per session (tokio task owning session state; commands in via mpsc, events out via a bounded shared ring). Turn execution:

1. Assemble context (`rw-context`): **prune runs here, deterministically** — the ADR-010 backward walk executes at the start of assembly (before the overflow check), and each erasure is persisted as a `ToolOutputPruned` event, so live context, `--resume`, `rw replay`, and golden transcripts always agree. User pins join the prune-protected set; user-evicted items count as already-pruned stop markers. Then: stable prefix → conversation → pending queued messages.
2. Stream from router; forward deltas as events.
3. On tool calls: permission check → hook `pre_tool` → (parallel where tools are read-only) execute → hook `post_tool` → results into next iteration. **Determinism rule**: regardless of completion order, tool events are *emitted and logged in tool-call index order* — parallelism is an execution detail, never visible in the event log, so golden-transcript replay stays byte-stable.
4. Loop until no tool calls; fire `turn_end` hooks; reconcile usage/cost; check compaction threshold.

Interrupt = cooperative cancellation token checked at every await point; partial output committed to the log with an `interrupted` marker.

Background shell execution is an owned session resource, not a detached task.
`bash.run_in_background` passes the already-approved request to the same command
executor used in the foreground under a write-denied sandbox, while a bounded process manager retains
redacted output and owns both the cancellation token and join handle.
`background_status`, `background_output`, and `background_kill` are ordinary
registered tools and can see only their authenticated `ToolContext` session.
Every registered tool receives an idempotent session-end cleanup hook; the
background manager cancels and awaits all children there. The executor's
process-group barrier, parent-death watchdog, and execution lease cover normal
shutdown, engine crashes, and recovery respectively. Active-resource observers
survive tool-registry filtering and block foreground shells, initialization,
fork/review/rewind, and execution-time workspace mutation until the background
job ends. Record/replay command-fixture modes reject background launch before
scheduling because their occurrence recorder is intentionally foreground-only;
historical event replay never invokes a tool at all.

### Mode state machine (`rw-core`)

The reserved built-ins are `discuss`, `plan`, and `execute`; the same registry
also accepts custom mode ids. Modes are data, not code: each definition contains
a system-prompt fragment, optional tool filter, and one of the three permission
policy overlays. Embedded built-ins pass through the same parser and registry as
extension-provided modes (dogfooding). Any plan-overlay → Execute transition
requires an approval event carrying the plan artifact. Durable custom-mode
events pin a semantic fingerprint so replay cannot silently change their policy.
The production runtime registry must therefore include every terminal tool named
by a built-in mode contract, including `submit_plan`; a test-only registry is not
evidence that a mode is usable in local or hosted production composition.

### Subagent orchestrator (`rw-core`)

Subagent = a full child session with its own event log, restricted tool registry, its own context budget. Parent holds a handle; child events are re-broadcast to the parent's *client* tagged with the child id (TUI shows nested progress) — **display-only, never persisted in the parent's log**. The parent log contains exactly: the `spawn_agent` tool call, `SubagentSpawned`, and `SubagentFinished` + tool result, all in tool-call index order per the determinism rule — parallel children completing in any order cannot perturb it. `rw replay` re-derives nested progress only from child ids authenticated by those durable spawn events; child logs use the same no-symlink event-log boundary and replay has explicit depth, session-count, event-count, per-event, and aggregate-byte ceilings. Worktree isolation delegates to `git worktree` via `rw-sandbox` path rules.

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
- **Record/replay middleware** wraps every provider: `--record` writes fixtures and the replay provider serves them back for tests. Built-in adapters retain and reparse their supported wire frames. RPC provider plugins use `WireMode::NormalizedReplay`, so their fixtures replay normalized provider events rather than an arbitrary plugin-specific wire dialect.
- Provider endpoint, proxy, and credential-reference settings are user-scoped security-sensitive configuration. Project files may select user-defined provider/model candidates, but cannot define or redirect a provider connection.
- The configurable adapter and wire-mode enums remain closed: Anthropic, OpenAI
  Chat/Responses, ChatGPT subscription, GitHub Copilot, and OpenAI-compatible
  Chat/Responses. Compatible adapters expose typed gateway controls for static
  and credential-backed headers, primary auth header scheme, extra query/body
  fields, a `{model}` path template, and model-id remapping. They do not make an
  unknown streaming/error wire dialect a configuration string.
- The RPC protocol supports bounded `provider/models` discovery and host-mediated
  `provider/http` authentication. Model capabilities, context/output limits,
  and optional pricing flow into the live catalog and accounting path. The
  plugin names an approval-fingerprinted credential reference; the host alone
  resolves, registers, and applies the secret.

### Context engine (`rw-context`)

- `ContextAssembler`: builds the prompt with a **frozen prefix** (system, tools, project docs) hashed each turn; a test asserts the hash is stable across turns of the same session unless config changed.
- `Budgeter`: local token estimates per block, reconciled with provider-reported usage; drives meters and the compaction trigger.
- `Pruner` + `Compactor`: **ADR-010 is the contract** (backward-walking prune with a 40k-token protection window and 20k minimum reclaim; the default reserve is the smallest of 20k, the model output limit, and half of the context window; summary generated by the `compaction` agent as an in-conversation assistant message using the Goal/Instructions/Discoveries/Accomplished/Relevant-files template; provider-overflow replay of the last user message; synthetic auto-continue nudge). Invalid resolved window metadata disables automatic compaction instead of creating a zero threshold. The `pre_compact` hook contract can inject context strings or replace the summary prompt.
- `toon` module: serializer from `Value` to TOON with property-based tests (round-trip through a decoder) and a token-savings benchmark.

Context mutations identify a conversation by its immutable journal sequence and a
tool result by that sequence plus its block index. Provider tool-call aliases and
reused turn ordinals cannot redirect a pin, eviction, or prune. Mutation admission
revalidates the selected source against the effective canonical generation.

Canonical results and context working plans share a 512 MiB allocation owner.
Resident results and checked working plans use at most 384 MiB, preserving
128 MiB for a query to make progress. Query admission has a FIFO queue of at
most 64 callers and a 30-second deadline; waiting does not start a worker.
Delivered results transfer only their measured bytes into resident ownership.
Context plans charge profiling metadata before scanning and grow to their
checked transformation allowance before copying or normalizing content. Their
high-water charge remains live with cached context and is released by its owner.

Compaction reads token- and byte-bounded pages into an owned rolling summary.
Large individual blocks use complete source/block/byte continuation through
UTF-8-safe summary fragments. Evicted and pruned payloads are removed before
serialization; pinned content must fit its explicit admission. All summary
requests contribute their actual usage and cost, and only a completed summary
transaction replaces the canonical conversation generation.

### Storage (`rw-store`)

- `sessions/<id>/journal/` — bounded sealed JSONL segments, `active.jsonl`, and a
  stable `writer.lock`. An append batch is synchronized before publication.
  Captured committed-prefix views support bounded cursor pages (ADR-029).
- Derived projection databases refuse database-wide crash repair. An unclean
  projection resets only its verified descriptor while retaining the writer lock,
  then catches up from bounded journal pages. Authoritative journal data and
  request-shape metadata are preserved.
- Connection-scoped acknowledgements are excluded from session journals/replay.
- `rw sessions verify <id>` checks every segment and typed event identity in an
  offline journal. Normal tail reads verify only referenced segments.
- Journal open validates the complete segmented layout before admitting reads or writes.
- `index.sqlite` — session listing/search tables and reconciled durable accounting.
  Normal opens admit the declared table definitions. Unsupported accounting schemas
  are rejected before writes. Explicit search rebuild
  replaces only its derived tables in one transaction; accounting and independent
  authoritative tables survive. An unreadable database is never deleted as a
  search-repair shortcut. Search projection retains bounded listing metadata and
  an exact journal-prefix digest. Message text and tool-result fields are separate
  source-qualified FTS documents; terms can match across documents in one session.
  Queries are plain whitespace-separated terms, all required, with punctuation
  interpreted by SQLite's tokenizer inside each field. Rewind deletes documents
  by their agent-turn identity in the same transaction as the source watermark.
  Catch-up reads at most 128 events and 16 MiB per page; incomplete projections
  stay out of search/list results until their captured source is covered.
  Live SQLite read transactions use a 1 MiB page cache and disabled memory mapping,
  so search does not copy the lifetime database or retain transcript bodies.
  Read-only handles cannot write stored rows; SQLite may maintain its ephemeral
  WAL/read-mark coordination files.
- Accounting facts and their time-prefix aggregates commit in one writer transaction.
  Session and global aggregates use a 49-level binary time index with exact u128
  sums; as-of and trailing-window reads visit a fixed number of nodes and convert
  only the selected totals to u64. A missing derived index rebuilds in resumable
  pages of at most 128 facts. Queries reject incomplete coverage. Accounting
  dispositions admit at most 1 MiB of encoded JSON before serialization; reads
  apply the same bound before allocating a database payload. Fact inspection and
  historical reports require an explicit row allowance and share a 16 MiB
  allocation allowance, checked against borrowed database values before decoding.
  Rewind and search rebuild never erase charged facts.
- Session `checkpoints/` namespaces retain per-turn manifests and rewind references.
  BLAKE3 blobs live in one application-storage owner keyed by the physical workspace,
  shared across primary/additional-root layouts. That owner admits at most 960 MiB
  of unique retained content plus one 64 MiB staging capture; equal content is
  deduplicated even at the retained limit. These are blob-content limits, not a
  claim about total workspace disk usage. A separate SQLite ledger has a 64 MiB
  page ceiling, 256 KiB page cache, at most 65,536 blobs and 1,024 namespace paths;
  its rollback journal and temporary reference tables have separate bounded storage.
  Source metadata admits at most 32 MiB encoded and a conservative 128 MiB decoded
  allocation per record. Cleanup also shares the capture operation's path, hash,
  and deadline bounds. Manifest traversal also admits at most 128 MiB of aggregate
  encoded source and 32 MiB of conservatively charged retained collections before
  growth. Turn selectors, rewind steps and review baselines share that allowance;
  review enforces its 1,024 unique-file limit during union, preserving each path's
  earliest baseline. Rewind reference publication/removal takes the same bounded
  writer exclusion as reclamation without changing blob-accounting state.
  Cold open performs no quota database reads or writes.
  Captures hold a cross-process workspace writer lease through manifest publication.
  Staging is reserved before writes; new retained content is admitted before blob
  publication. Interrupted operations reconcile before new admission. Reclamation
  validates every registered manifest and rewind reference before removing only
  unreferenced content; malformed or incomplete inventory fails closed. Referenced
  history is never evicted to make room. Namespace references remain durable;
  removal requires the same authority, and a missing registered namespace is an
  incomplete inventory. Empty fork stores have no registration until capture,
  so abandoning an uncommitted fork does not leave quota references. The initial
  quota ledger is atomically published, and new directory entries are fsynced
  before a publication becomes clean. Checkpoint namespaces reject unexpected
  blob directories; all captured content belongs to the shared workspace authority.
- Config precedence: built-in defaults ← `~/.rottweiler/config.toml` ← `.rottweiler/config.toml` ← env ← CLI flags. **Exception**: security-sensitive keys (`[permissions]`, safe-list, `[network]`/proxy, telemetry opt-in, update channel) are ignored at project level with a warning (05 Layer 0). Schema in `rw-types`, `rw config check` validates and prints effective config with provenance per key.

A rewind spanning multiple workspace roots has one session-level durable
coordinator decision. Per-root transactions are prepared first; only a fsynced
`committed` decision authorizes workspace application. Recovery discards a
`preparing` operation without changing any root and idempotently completes a
`committed` operation before appending its deduplicated conversation event.

Forking is a conversation operation, not a working-tree clone. The child copies
the exact durable event prefix and historical workspace-root generation, but it
uses the current shared workspace under the same execution lease as its parent.
Its checkpoint namespace starts empty, so review, rewind ownership, and spend
attribution cover only child work. A bounded private fork-operation journal is
fsynced before child materialization; child metadata is the storage commit
marker, and a completed record retains the exact authenticated request/result.
Startup recovery removes incomplete child trees or promotes committed records,
so a lost response or process death can be retried without creating a second
child. Fork idempotency is keyed by a bounded client-generated operation id,
not the connection-scoped client/request pair. The local TUI writes that pending
identity to a private per-session handoff before POST and retains it across
process restarts until the correlated typed completion arrives; retries may use
new transport credentials while the engine re-correlates the original child to
the new request metadata. The prepared journal also authenticates the exact
historical model, workspace-root generation, and root digest, so completed
operation recovery remains constant-cost and never rescans a child log or root
journal that may have grown substantially after the fork.

**Configuration path and merge contract.** `ROTTWEILER_HOME/config.toml` is the explicit
user-path override; otherwise an explicitly set
`$XDG_CONFIG_HOME/rottweiler/config.toml` is used, falling back to the
documented `~/.rottweiler/config.toml`. Environment keys use the `RW_` prefix,
and `rw config check --set key=value` supplies the CLI layer. Tables merge by
leaf, model-alias maps merge by alias, and list values replace rather than
concatenate. Provenance is recorded per rendered leaf. Project-level
security-sensitive sections are rejected before merging, so they never become
effective even transiently.

**Provider and network contract.** `[providers.<name>]` holds an adapter kind,
optional endpoint/path template, API-key environment/credential references,
static and credential-backed headers, primary auth presentation, extra query/body
fields, model-id mappings, per-model USD pricing, and an optional
provider-specific proxy. `[models].aliases` remains the provider-blind ordered
`provider/model` routing table; `[models].thinking` maps aliases to
`off|low|medium|high`. Proxy authentication is configured as a non-secret
username plus a `proxy_password_credential` identifier; the password resolves
as one logical key inside Rottweiler's single versioned owner-private credential
file (mode 0600 on Unix) and is never rendered. Production does not link or call
an operating-system credential-store backend, including during provider authentication.
The global `[network]` form uses the same fields. Provider definitions and all
proxy/authentication fields are user-scoped and ignored in project config.
Gateway `base_url` values cannot contain credentials, query, or fragment;
`extra_query` is the only typed query addition, and primary credentials cannot
be placed in the query. `Host`, hop-by-hop, invalid, duplicate, and conflicting
primary-auth headers are rejected. Engine-controlled body fields cannot be
replaced. Anthropic, ChatGPT subscription, and Copilot fixed transports reject
gateway request overrides.
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

The `openai_codex` kind is intentionally separate
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

The `github_copilot` kind is the other isolated subscription profile. Released
builds pin the audited public Copilot CLI-compatible OAuth device client id at compile time;
configuration cannot supply a client id, API key, generic OAuth endpoint, or
Copilot base URL. `rw auth login github-copilot` shows GitHub's verification URI
and user code, polls the device grant with expiry/backoff and explicit
cancellation, then stores one Rottweiler-owned logical token entry. It never
reads `gh`, Copilot CLI, VS Code, or OpenCode credentials. The factory registers
that token with the shared redactor before any request. The credential records
the OAuth client id that issued it; production resolution requires the current
build's pinned client id to exist and match exactly. Loopback fixtures
instead supply an explicit non-production test identity. One async lazy catalog
runtime is shared by all configured models for the logical provider: the first
inference fetches `/models`, rejects 401/403, policy-disabled, missing, or
capability-incomplete models, selects Messages before Responses before Chat from
the discovered endpoints, and only then sends inference to the fixed
`api.githubcopilot.com` origin. Startup therefore remains synchronous and fast;
pre-discovery claims are only routing hints. The outer model binding enforces the
exact model id but delegates vision, thinking, tool, and limit validation to the
discovered inner adapter, so supported features remain usable and absent features
still fail before inference. `ProviderRuntime::model_metadata` exposes those
authenticated capabilities and rates asynchronously through a provider-neutral
contract. Copilot rates carry an explicit AI-Credit accounting unit and nominal
micro-dollar-per-credit conversion; they are never presented as an ordinary API
dollar `$0` route.

Provider-neutral request invariants are not capabilities and never wait for
discovery: exact model binding and tool-choice consistency (`required` needs at
least one tool; `named` must match an exposed tool) fail before the lazy catalog
can open `/models`. Only model-dependent feature checks are deferred.

Pricing resolution is whole-record rather than field-by-field: explicit user
configuration wins over authenticated provider-discovered metadata, which wins
over models.dev enrichment. The resolved model retains that source. `rw config
check` prints user-declared pricing records with `source = user_config`; live
provider/models.dev selection occurs at provider composition. ChatGPT and
Copilot routes discard dollar-pricing records and retain `SubscriptionQuota` and
`AiCredits`, respectively.

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
after durable persistence. Credential persistence warnings produced by a
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

Opaque reasoning continuations belong to the producing adapter. The provider
operation binds each payload to the provider name, concrete model, and a digest
of adapter configuration and authority. Native plugins derive that digest from
the exact approved, initialized generation. Lazy activation remains owned while
the digest is resolved. The operation rejects mismatched or malformed state
before inference admission, then passes only the adapter payload to the provider.
Returned continuations are bounded to 256 KiB including their envelope. A provider
without declared provenance cannot consume or emit opaque state. Recordings retain
the provenance explicitly, so replay reconstructs the same conversation values
without network access. Failover cannot transfer a continuation to another route.

### TUI (`packages/tui`, OpenTUI)

Built on OpenTUI (per ADR-001), which supplies the retained component tree and the Zig renderer (double-buffered cell diffing, damage-tracked partial redraws) — do not reimplement rendering primitives it already provides. Our layer on top:
- **Transcript** as a virtualized list over the session event log — only visible messages mount, so 10k-message sessions scroll at full speed; streaming deltas append to the tail component without re-laying-out history.
- **SSE client** with reconnect + event-sequence resync (events carry monotonic ids; on reconnect the TUI replays the gap from the engine).
- **Replay presentation boundary** is the engine's `session_replay_completed` marker. The TUI reduces historical events immediately but suppresses their live-only presentation effects until that marker arrives; stream termination only performs idempotent cleanup for an incomplete replay.
- Components: markdown/syntax-highlighted blocks, collapsible tool calls, diff accept/reject view, fuzzy pickers, question prompts, nested subagent progress, status line.
- The application composes controllers for input and focus, session navigation,
  child drafts, picker catalogs, provider interactions, settings, permissions,
  themes, submission, and client restoration. Component construction consumes
  explicit interaction ports. Each controller owns its pending requests and
  timers; a reply may update only the session scope that issued it. Handwritten
  files are capped at 1,500 lines, and TUI typechecking rejects unused declarations.
- `ClientCommand`, `EngineEvent`, and the shared IR are owned by the Rust types in
  `crates/rw-types`. `cargo xtask codegen` generates `protocol/types.ts`, the JSON
  schemas, and the cross-language fixtures from those types. The TUI imports the
  generated TypeScript projection; CI rejects drift.
- `EngineEvent::delivery()` owns durable, transient, and connection-scoped event
  lifetime. Protocol codegen derives a complete TypeScript delivery map from the
  Rust event schema, and the reducer consumes that projection. Unknown wire
  discriminators are rejected at ingress.

## Cross-cutting behaviors

- **Errors**: `rw-types::Error` with categories (Provider, Tool, Sandbox, Config, Extension); user-facing messages are actionable ("model X hit rate limit, failing over to Y"), full chains in the debug log.
- **Cancellation**: every async boundary takes a `CancelToken`. No detached tasks without a registered owner.
- **Redaction**: a single `Redactor`, with **scoped aggressiveness**: content entering model context (file reads, `bash`/`webfetch` output) is redacted only via *known secrets* (registered environment values and credential-file entries) and strict key-format regexes — no entropy heuristic there, because false positives corrupt what the model sees and cause wrong edits. The entropy heuristic applies only at export/share boundaries, where a false positive is cosmetic.
- **Time & randomness** injected via traits — required for deterministic replay.

### Continuable child ownership

One orchestration owner admits at most 256 live or continuable children across its tree. A child keeps that slot after a turn finishes; a proven close releases it. Startup and failed cleanup retain the same slot, so cancellation cannot exceed the admitted ownership. Canonical child history and artifacts remain queryable independently of live child actors.

Private recovery records have explicit fields and bounded JSON decode admission. Recovery visits record pages of at most 16 entries and 16 MiB of admitted allocation, retains only bounded identity/fingerprint references between parents, then checks each record against its captured fingerprint before rebinding. It does not retain all child policy bodies while traversing the tree.

Recovered child sessions hold an admitted resume recipe and bind their actor only when a turn starts. A factory allows 32 MiB of recovery policy preparation; shared tool grants are retained by reference. Preparation survives dropped callers, and close waits for its actual owner before releasing resources. Child progress previews are capped at 256 KiB; larger events send a null preview with their canonical child sequence so clients can read the source. Follow-up turns subscribe at the live source boundary.

Child display publishers coalesce at most one queued observation per admitted child, with an 8 MiB prepared-allocation allowance shared by the publisher. Both tool-launched and hosted children use the same slot semantics. Saturated delivery marks the next canonical source fence; it never waits for display capacity before settling child effects. Hosted lifecycle records and observations enter the same actor queue, with durable terminal acknowledgement releasing the active progress binding.

### Recorded request shapes

Each session stores request-shape metadata in a private indexed database. A row
binds an immutable canonical context source and turn to a deduplicated tool/cache
profile and streaming request fingerprint. It contains no conversation bodies.
Provider dispatch waits for the context source to commit, then an owned write
records the first provider request for that source; a reused turn has a
separate source identity. Historical prompt verification selects that exact source,
and inspection selects profiles from the effective canonical prompt index.
A different workspace or provider configuration cannot impersonate the recorded
request. Missing or inconsistent metadata makes the historical prompt unavailable.

The database owns a 256 KiB page cache. Reads select one bounded profile directly;
startup and writes never scan or materialize lifetime request metadata. Profile
JSON is admitted at 4 MiB encoded and 16 MiB decoded before typed allocation.
Direct profile decoding charges actual object/array structure and typed fields;
scalar values do not each receive an unrelated map allocation charge.
