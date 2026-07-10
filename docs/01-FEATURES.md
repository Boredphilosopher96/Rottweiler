# 01 — Feature Specification

Every feature Rottweiler ships, grouped by area. Features marked **[gap]** were not in the original brief and were added to make the harness competitive — remove consciously, not by omission.

## 1. Interaction model

- **Three modes**, switchable at any time (Shift+Tab cycle, or `/mode`):
  - **Discuss** — no tool calls that mutate anything; read-only tools allowed. For rubber-ducking and Q&A about the codebase.
  - **Plan** — read-only tools; the model finishes by calling the built-in `submit_plan` tool, whose payload is the **plan artifact**: `{ title, summary_md, steps: [{ description, files_touched?, verification }], open_questions }`. The client answers with `ApprovePlan { revisions? }` or rejection feedback; the approved artifact is pinned into Execute-mode context (survives compaction) and `/plan` re-displays it.
  - **Execute** — full tool access under the active permission policy.
- **Ask-user questions**: the model can pose structured multiple-choice/free-text questions mid-turn (an `ask_user` tool); TUI renders a picker, CLI mode renders a prompt or auto-answers per policy.
- **Steering & interrupt** **[gap]**: Esc interrupts the current turn cleanly (tool calls cancelled, partial state preserved); messages typed mid-turn are queued and injected at the next turn boundary.
- **Message editing / rewind** **[gap]**: rewind the conversation to any prior turn; file checkpoints (see §7) restore workspace state alongside.
- **Bash escape**: `!cmd` runs a shell command with the **real TTY handed over** (TUI suspends raw mode): interactive commands — login flows, REPLs, editors, sudo prompts — just work. The agent is **blocked until the process actually exits**: Ctrl+C/signals go to the child, not the harness, and an interrupted-but-still-running process keeps control; the agent never resumes against a half-finished command. During handover, background-process output is buffered (never written to the surrendered TTY) and MCP servers keep running. Output (redacted) lands in the transcript as user-provided context.
- **$EDITOR composition** **[gap]**: a keybinding opens `$EDITOR` to compose or edit long prompts; saved buffer becomes the message.
- **@-file mentions & fuzzy picker** **[gap]**: `@src/main.rs` attaches a file reference; `@` opens a fuzzy finder over the repo.
- **Multimodal input** **[gap]**: paste/attach images (screenshots of UIs, error dialogs) for vision-capable models.

## 2. Slash commands & skills

- **Built-in `/` commands**: `/help`, `/mode`, `/model`, `/compact`, `/cost`, `/context`, `/init`, `/deep-init`, `/agents`, `/mcp`, `/permissions`, `/resume`, `/fork`, `/rewind`, `/review`, `/add-dir`, `/export`, `/theme`, `/config`, `/memory`, `/trust`.
- **Custom commands**: Markdown files in `.rottweiler/commands/` (project) and `~/.rottweiler/commands/` (user) with frontmatter (`description`, `model`, `allowed-tools`, `args`). Body is a prompt template with `$ARGUMENTS`, `$1..$n`, and `` !`cmd` `` pre-execution interpolation.
- **Skills** **[gap]**: SKILL.md-standard directories (frontmatter + instructions + bundled scripts/resources), loaded lazily — only name+description in context until invoked. Skills and commands share one registry; a command is just a skill with no resources.
- **Extension-provided commands**: plugins can register commands with full tool access (see 04-EXTENSIBILITY).

## 3. Context engine

- **Live meters**: status line shows context used / budget, session cost, and cache-hit rate. Per-turn cost and token deltas render **inline in the transcript** (cost transparency is a headline feature, not a buried stat).
- **Context surgery** **[gap]**: `/context` is not read-only — it's an interactive inspector: per-item breakdown (system prompt, AGENTS.md, tools, MCP, messages, tool results) where the user can **evict, pin, or summarize individual items** (that 40k-token log dump, a stale MCP result) without nuking the whole session. Evictions are events (undoable via rewind).
- **Compaction — 1:1 port of opencode's strategy** (deliberately; see ADR-010 for the exact mechanics distilled from `opencode/src/session/compaction.ts` + `overflow.ts`):
  - **Prune first, summarize later.** Pruning walks the transcript backward, skips the most recent two user turns, protects the newest ~40k tokens of completed tool outputs (and a protected-tools list, e.g. skills), and erases the output of everything older — but only when ≥ ~20k tokens would be reclaimed. Stops at the last summary or already-pruned part. Free (no model call); runs as a deterministic step at each turn's context assembly, persisted as `ToolOutputPruned` events (02-ARCH) so replay and resume stay exact.
  - **Overflow trigger**: compaction fires when session tokens ≥ usable window − reserved buffer, where reserved = `min(20k, model max output)` (configurable). Manual `/compact [instructions]` anytime.
  - **The summary is a conversation message**, not a side artifact: a dedicated `compaction` agent (own model alias, defaults to the session's model) generates it against the prior messages (media stripped) using the opencode markdown template — **Goal / Instructions / Discoveries / Accomplished / Relevant files & directories** — written as "a prompt for the next agent to continue," in the user's language, no tools. Post-compaction context is everything from the summary message forward; full history stays in the event log.
  - **Provider-limit overflow variant**: if the *request itself* blew the provider limit, rewind to before the last real user message, compact everything prior, then **replay** that user message after the summary (media attachments become text placeholders).
  - **Auto-continue**: after automatic compaction, a synthetic user message nudges: "Continue if you have next steps, or stop and ask for clarification" (suppressible via hook).
  - Hook integration: `pre_compact` can inject context or replace the summary prompt entirely (mirrors opencode's plugin trigger).
  - Our only additions, guarded so they don't change the strategy: **conversation-resident** pinned items (the approved plan artifact, user-pinned messages) re-enter after the summary in a defined order — summary → pins → replayed user message. AGENTS.md is *not* a post-summary pin: it lives in the stable prefix, never left context, and re-injecting it would duplicate it. The assembler keeps the summary at a cache-stable position.
- **TOON / compact encodings**: structured tool results (search hits, directory listings, diagnostics, MCP JSON responses) are encoded in TOON (token-oriented object notation) or equivalent tabular form instead of pretty JSON. Measured requirement: ≥30% token reduction on structured payloads vs pretty-printed JSON.
- **Prompt-cache-aware ordering**: context assembled as [system → tools → AGENTS.md/skills index → conversation], with mutable data (time, meters) excluded from the cached prefix. Cache breakpoints set per provider rules. Cache hit rate is surfaced, and a CI test asserts the prefix is byte-stable across turns.
- **Token counting**: local estimator for live meters; reconciled against provider-reported usage each turn.

## 4. Model router (provider-blind)

- **Unified message IR**: one internal representation for messages, tool calls, thinking blocks, images, citations. Providers are adapters: Anthropic Messages, OpenAI Chat/Responses, Gemini, OpenAI-compatible (OpenRouter/Ollama/vLLM/anything).
- **Model aliases**: config maps roles to models — `big`, `fast`, `plan`, `compact`, `title` — e.g. `big = "anthropic/claude-opus-4"`. All internal features reference roles, never concrete models.
- **Fallback chains & failover** **[gap]**: per-alias ordered fallback list; automatic failover on 429/5xx/timeout with jittered backoff; sticky failback.
- **Cost tracking**: per-turn and per-session cost from a bundled, updatable pricing table (`rw models refresh` pulls latest from models.dev or configured source — never hardcode prices).
- **Budget caps** **[gap]**: soft warning and hard stop at configurable per-session / per-day spend.
- **Auth**: API keys via env/keychain; OAuth flows for providers that support subscription auth. Keys never written to session logs. AWS Bedrock / Google Vertex adapters (auth-divergent enterprise routes) are **explicitly post-v1** — the OpenAI-compatible adapter is the v1 escape hatch via a gateway.
- **Thinking / reasoning-effort control** **[gap]**: a user-facing dial, not just an adapter param — per-alias config (`thinking = off|low|medium|high`) and per-session override in `/model`; adapters map it to each provider's mechanism (thinking budgets, reasoning-effort params) and it round-trips through record/replay.
- **Local models**: first-class OpenAI-compatible local endpoint support (Ollama, LM Studio); router must degrade gracefully when a provider lacks tool-calling or caching.
- **HTTP(S) proxy, three scopes**: `[network] proxy` applies to **everything** outbound (providers, `webfetch`, remote MCP, model-table refresh, self-update); `[providers.<name>] proxy` overrides it for a **specific provider**; setting only per-provider proxies covers the "some providers via proxy, rest direct" case. `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` env vars are honored as the lowest-precedence layer. Proxy credentials come from the keychain, never inline in config files, and never appear in logs.

## 5. Tools (built-in)

`read`, `write`, `edit` (string replace: exact match first, whitespace-normalized fallback; ambiguous matches fail with candidate locations listed — never guess), `multi-edit`, `grep` (ripgrep engine), `glob`, `ls`, `bash` (sandboxed, see 05-SECURITY), `webfetch` (URL → markdown; size-capped, injection-dampened per 05, ask-per-new-domain with remembered approvals, SSRF-guarded, honors the proxy config), `websearch` **[gap]** (web search: provider-native search where the model supports it, else a configured search API; results through the same egress policy), `todo` (task list the model maintains), `ask_user`, `spawn_agent` (§6). All tools:
- Emit structured results (TOON-encodable).
- Declare a capability manifest (reads-fs / writes-fs / network / exec) consumed by the permission engine.
- Are registered through the public tool registry (dogfooding rule).
- **Code intelligence — both tiers ship in v1**:
  - **Tree-sitter symbol index** **[gap]**: always-on, zero-config index (definitions, references, symbols) built at session start and updated incrementally on edits; exposed as a `symbols` tool. The 80/20 of code intelligence with no per-language server management.
  - **LSP integration**: auto-start language servers found on PATH; diagnostics injected after edits (compile errors surface without a build); go-to-def/references/rename exposed as tools. Full v1 feature, not flagged; degrades gracefully to the tree-sitter tier when no server is available.
- **Formatters & linters via hooks**: a declarative `[toolchain]` config tier — `formatter = "cargo fmt"`, `linters = ["cargo clippy"]` per language/glob — that registers built-in `post_tool` hooks: after every edit/write, the formatter runs on the touched file and linter diagnostics are appended to the tool result. Zero plugin code required; implemented on the same hook API plugins use (dogfooding rule). Details in 04-EXTENSIBILITY.
- **Streaming tool output** **[gap]**: long-running `bash` calls stream stdout/stderr live into the TUI while executing (a 5-minute build must never look dead); the model receives the (size-capped, tail-biased) final output.
- **Background processes** **[gap]**: `bash` supports `run_in_background`; a process manager tracks them, streams output on request, kills on session end.

## 6. Subagents & orchestration

- **Agent definitions**: Markdown files (`.rottweiler/agents/*.md`) with frontmatter: `name`, `description`, `model` (alias), `tools` allowlist, `permission-mode`, plus a system-prompt body. Built-in agents (`explore`, `plan`, `general`) defined the same way.
- **`spawn_agent` tool**: parent spawns children with a task prompt; children run in parallel; results return as tool results. Depth limit (default 2) and concurrency limit (default 4) configurable.
- **Isolation options** **[gap]**: subagents can run in a git worktree (auto-created, auto-cleaned if untouched) so parallel agents don't trample the working tree. **Merge-back contract**: a worktree subagent's changes return to the parent as a diff artifact; the parent applies it (3-way apply) as an explicit tool step, and conflicts surface in the tool result for the parent to resolve — never silent merges.
- **Subagent return schema**: what the parent receives is defined — final assistant text + touched-files manifest + diff artifact (if isolated) + usage/cost — so orchestration context cost is predictable.
- **Continuable agents** **[gap]**: subagent sessions persist; parent can send follow-up messages to a completed child instead of respawning cold.
- **Workflows**: a declarative pipeline format (TOML) chaining agents/commands with sequential/parallel steps and simple conditions — e.g. `plan → parallel(implement, write-tests) → review`. Runs headless or interactive.

## 7. Sessions, persistence, checkpoints

- **Event-sourced sessions**: every session is an append-only JSONL event log (documented schema) + SQLite index for listing/search. `--resume` / `--continue` restore exactly.
- **Forking**: branch a session from any point (`/fork`).
- **File checkpoints** **[gap]**: before each mutating tool call, affected files are snapshotted (content-addressed store, like a shadow git). For `edit`/`write` the target is known a priori; **for `bash` it isn't**, so bash uses a git-assisted pre/post strategy: before execution, snapshot currently-dirty tracked files (cheap: `git status` + hash-object of the dirty set); after execution, a post-scan identifies what changed — files whose pre-state existed nowhere (untracked, never read, never snapshotted) are recorded as **unrestorable** and `/review`/`/rewind` surface that honestly instead of pretending. `/rewind` restores conversation + files together.
- **Session diff review** **[gap]**: `/review` shows the **cumulative diff of everything the session changed** (computed from the checkpoint store), with per-file accept/revert — the natural end-of-session ritual before committing.
- **Session replay** **[gap]**: `rw replay <session>` renders a past event log through the TUI as if live — for demos, TUI debugging, and visually reproducible bug reports. Falls out of event sourcing.
- **Export**: transcript → Markdown/HTML/JSON. Session format is a documented public schema.

## 8. Init & project intelligence

- **`/init`**: analyzes repo (build system, layout, conventions, test commands) and writes AGENTS.md.
- **`/deep-init`**: walks the tree, produces per-directory AGENTS.md for major subsystems with an index in the root file; skips vendored/generated dirs; respects a size budget per file.
- **AGENTS.md standard**: reads AGENTS.md (and CLAUDE.md as fallback) from repo root, subdirectories (loaded when files there are touched), and the user level. Nested files merge child-over-parent.
- **Open `.agents` discovery** (ADR-014): for everything user-authorable — agents, commands, skills, plugins config, AGENTS.md — the lookup order is **`~/.agents/` first, then `~/.rottweiler/`** at user level, and **`.agents/` first, then `.rottweiler/`** at project level; first match by name wins. The portable open-standard location is the source of truth; `.rottweiler/` holds harness-specific overrides and private state (config.toml, sessions, keys).
- **Multi-root workspaces** **[gap]**: `--add-dir <path>` (and `/add-dir`) extends the workspace to additional roots — tools, permissions, sandbox write-scope, and AGENTS.md discovery all honor the extended set. Day-one need for monorepo-adjacent repos.
- **Memory** **[gap]**: `/memory` manages a persistent per-project memory file the agent can read/write across sessions (distinct from AGENTS.md, which is human-owned).

## 9. MCP

- **Client**: stdio + HTTP/SSE transports, OAuth for remote servers. Config in `.rottweiler/mcp.toml` + user-level. **Tools, resources, and prompts** are all supported: resources readable as context attachments, server prompts surfaced as slash commands.
- **Capability classification**: MCP tools carry no trustworthy capability manifest, so they default to the most restrictive class (**network + exec — always `ask`**); per-server or per-tool overrides let the user downgrade known-safe tools to read-only. The permission engine never treats an MCP tool as benign by default.
- **Context-overload control** (this is the differentiator):
  - **Deferred tool loading**: MCP tools are indexed but only name+one-liner enter context; full schemas load on demand via a built-in `tool_search` tool.
  - Per-server enable/disable at runtime (`/mcp`), per-agent tool allowlists.
  - MCP responses TOON-encoded and size-capped, with overflow spooled to a file the model can grep.
- **Server mode** **[gap]**: Rottweiler can expose itself as an MCP server (offering its tools/sessions) so other agents can drive it.

## 10. CLI / headless / SDK

- **Print mode**: `rw -p "prompt"` → runs headless, prints result; `--output-format json|stream-json` for scripting; exit codes reflect success.
- **CI-safe policies**: non-interactive permission policy (`--permission-mode`), max-turns, max-budget flags.
- **Stdin piping**: `git diff | rw -p "review this"`.
- **SDK surface**: `rw-core` published as a library crate — the TUI is proof the SDK is sufficient (dogfooding rule).
- **Scriptable server**: `rw serve` exposes the engine over an HTTP+SSE API (what the TUI itself uses in client/server mode).
- **Remote engine** (core requirement, not post-v1): `rw --remote <host>` runs the engine where the code lives and the TUI locally — over an SSH-forwarded socket by default. The client/server split exists precisely so any UI can attach to any engine; the protocol must never assume localhost (auth token on every connection, no machine-local paths leaked in events, reconnect/resync built in).

## 11. TUI

- **OpenTUI** (TypeScript/Bun, opencode's stack) as the frontend, talking to the Rust engine over the client protocol (ADR-001): OpenTUI's Zig renderer gives damage-tracked partial redraws, 60fps streaming, no full-screen redraw per token. Shipped as a Bun-compiled self-contained executable spawned by `rw` — users never install Node/Bun.
- Markdown rendering with syntax highlighting; unified diff view with accept/reject for edits; collapsible tool-call blocks.
- Fuzzy pickers (files, commands, models, sessions); themes; configurable keybindings incl. vim mode **[gap]**; image preview where terminal supports (kitty/iTerm2 protocols).
- Status line: mode, model alias, context %, session cost, cache-hit %, git branch. Extensible via config script.
- **Desktop notifications** **[gap]**: notify when a long turn finishes or the agent blocks on an approval/question while the terminal is unfocused (macOS/Linux native, configurable).
- Scrollback that never drops content; copy-friendly (no decorative borders inside code blocks).

## 12. Safety & security

See 05-SECURITY.md. Headlines: **folder trust gate** (untrusted repos load no project config/commands/plugins until blessed), per-command sandbox (Seatbelt/Landlock), three-tier permission engine (allow/ask/deny with pattern rules), secret redaction in logs and transcripts, no default telemetry, hooks for org policy enforcement.

## 13. Observability **[gap]**

- Structured tracing (`tracing` crate) with `--debug` spool to file.
- **Prompt transparency**: `rw prompt dump [--turn N]` prints the exact assembled request (system prompt, tools, context order, cache breakpoints) that was/would be sent — the debugging tool for token economy and plugin authors.
- Optional OpenTelemetry export (opt-in) for teams.
- `/cost` and `rw stats`: per-session and historical spend, tokens, cache savings, tool-use counts, cost attribution (main turns vs compaction vs subagents).

## 14. Guardrails **[gap]**

- **Doom-loop detection**: N consecutive failing tool calls, or repeated near-identical calls, triggers an interruption — the engine injects a "you appear stuck: rethink the approach or ask the user" message; a second trip pauses the turn for user input. Thresholds configurable.
- **Spend-rate alarm**: alerts on $/minute burn rate, not just cumulative budget; catches runaway loops before the budget cap does.
- **Max-turns / max-duration** per session and per subagent, enforced by the engine.

## 15. Adoption & lifecycle **[gap]**

- **`rw import`**: migrate from Claude Code / opencode / pi — custom commands, MCP server configs, CLAUDE.md→AGENTS.md, memory files, and **shell-command hooks** (Claude Code settings-style hooks map onto `hooks.toml`, see 04). Mostly file transforms; the difference between "tried it" and "switched."
- **`rw doctor`**: first-run and troubleshooting diagnostics — auth status, provider reachability, sandbox capability probe, terminal feature detection.
- **Self-update**: `rw upgrade` with stable/beta channels; release notes shown on first run after update.

## Explicitly out of scope for v1

IDE plugins, web UI, cloud-hosted execution, extension marketplace, Windows-native sandbox (Windows runs with sandbox=off + warning; WSL fully supported), voice input.
