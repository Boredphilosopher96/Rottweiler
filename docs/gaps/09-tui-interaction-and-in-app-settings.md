# 09 — TUI interaction failures & the files-vs-app design gap

Maintainer-reported symptoms from a live TUI session (2026-07-12): tool calls don't work, `/` commands don't work, the bottom bar (context %, pricing, cache) is dead, and `/providers` shows a single entry. Plus a design directive: **simple configuration must be doable from the app, opencode-style — not by editing files.** Auto-routing (GAP-08-05) is explicitly parked until the catalog and these basics work.

## GAP-09-01 — Tool calls don't work in the interactive TUI — **P0 [user-reported; code-traced suspects]**

In interactive (non-headless) mode the permission default is `ask`, so **every mutating tool call blocks on a `ToolApprovalNeeded` → approval-panel → `approve_tool` round trip**. The wiring exists (`app.ts:263 onApproval`, `app.ts:1439 #approve`, `panels.ts` approval select), but if any link fails — the panel not receiving focus, the event not projecting, the `approve_tool` command being rejected (e.g. the client isn't the recognized driver), or the engine binding approvals to a `proposal_id`/hash that doesn't match (`approvalBinding`, `app.ts:1803` requires `proposal_id`+3 hashes or returns null) — the turn stalls silently and every tool appears dead.

**Verify in this order:**
1. Run a mutating prompt in the TUI, then `rw replay <session> --jsonl` — is there a `tool_approval_needed`-class event? If yes, the engine side works and the fault is panel focus/rendering or the approve round trip.
2. Check whether `approve_tool` is rejected with a driver/lease error (the TUI must hold the driver lease; a rejected approval must surface, not vanish).
3. Check the `approvalBinding` null path — if the diff payload lacks the binding fields the panel may render a deny-only state (`panels.ts:429` "A truncated change cannot be approved").

**Fix principle:** a blocked-on-approval turn must be *loudly visible* (status line: "waiting for approval"), and any rejected/failed approval command must render an error — never a silent stall.

## GAP-09-02 — `/` commands appear broken in the TUI — **P0 [user-reported; code-traced]**

Three compounding causes, all traced:

1. **The slash menu is fed by `list_commands`, whose failures are silently swallowed.** Typing `/` opens the anchored picker (`app.ts:1268`) populated from `#state.commands` ← `list_commands` ← `command_descriptors()`. The initial projection commands are "opportunistic" — `runtime.ts:533`: *"These read projections are opportunistic. Their individual command [failures are ignored]"*. If `list_commands`/`list_models`/`list_sessions` are rejected (bad session binding, lease, timing), the pickers are just **empty forever** with no error. Same mechanism explains empty/thin model and session pickers.
2. **The menu can only ever contain built-ins** (GAP-08-03): `command_descriptors()` returns `builtin_command_registry()` only — custom commands, skills, and plugin commands never appear.
3. **Only 4 commands are handled client-side** (`parseSessionAction`, `app.ts:1773`: `/review`, `/models`, `/providers`, `/fork`); everything else rides `send_message` and relies on the engine's slash-intercept (`engine.rs:7353`) plus `command_finished` rendering (`reducer.ts:824`). That path is fine *in design*, but it means one silent failure in the event stream makes every command look like a no-op.

**Fix principle:** projection failures must surface (a one-line "couldn't load commands: <reason>" in the picker), and the command list must merge built-in + custom + skills + plugin commands with source tags.

## GAP-09-03 — Bottom bar: context %, cost, cache all dead — **P0 [confirmed; same root as GAP-02-01]**

The status line shows `ctx — │ $— │ cache —` because `context_usage_updated` carries `usable_tokens: 0` — the subscription provider path hardcodes `max_context_tokens: None` (`provider_factory.rs:1894`) and never consults the pricing table. Cost shows `—` because subscription cost is "quota-unavailable" and the status line has no quota/token fallback rendering; cache % needs provider-reported usage which is present but has nothing to display against a zero window. **One fix (resolve subscription model capabilities from the catalog) re-lights ctx and cache; cost needs a second small fix: when cost is `subscription_quota`, render used tokens/credits instead of `$—`.**

## GAP-09-04 — `/providers` shows one entry; should show all available options — **P1 [confirmed + design]**

Two layers:
- **Mechanical (GAP-08-02):** the picker derives providers from alias references (`app.ts:1032`); the maintainer's aliases are all `openai/*`, so exactly one appears.
- **Design (the real ask):** the maintainer expects the provider surface to list **all supported/known providers with auth state** — like opencode, where the provider list comes from the live catalog (models.dev set + configured credentials) and you can pick one and authenticate *in the app*. Rottweiler has rich auth machinery (`rw auth`, device flows, keychain vault) but it is **CLI-only**; the TUI can only render what aliases mention.

**Fix:** engine exposes a provider inventory — `{ name, auth_kind, authenticated?, reachable?, model_count }` for configured providers *plus* known-supported providers in an "available to set up" section — and the TUI provider picker drives an in-app auth flow (device-code display for Copilot/ChatGPT, key prompt for API providers) instead of pointing users at config files.

## GAP-09-05 — Settings are file-only; simple use cases must be manageable from the app — **P0 [design]**

The maintainer's directive: *"our settings are supposed to be changed and done from the app for all simple use cases. But now it is only loaded from files. That is stupid."* Current state: model aliases, thinking levels, provider auth, compaction toggles, themes, keybindings, MCP servers, permission defaults — all live in `~/.rottweiler/config.toml` + CLI subcommands. The TUI palette has a few read-only settings items (`permissions.list`, `trust.status`, `mcp.manage` prefill) but **no write path for configuration at all**.

opencode parity means, at minimum, in-app (TUI) flows that **persist**:

| Setting | opencode surface | Rottweiler today | Needed |
|---|---|---|---|
| Model selection | `/models` picker, persists per project | picker exists, alias-only, session-only | pick concrete model from dynamic catalog; persist choice (project scope) |
| Provider add/auth | `opencode auth login` (interactive picker) | `rw auth` CLI only | `/providers` → pick → in-app auth flow → saved to vault |
| Thinking level | model variant cycling | config file only | per-session toggle in `/model` picker, persistable |
| Theme | `/theme` picker, live preview | `themes/*.toml` files | in-app picker writing user config |
| Permissions | n/a (config) | `/permissions` shows JSON | interactive rule add/remove already partially exists (`/permissions add`); needs UI, not JSON dumps |
| MCP servers | config + `/mcp` toggles | `rw mcp` CLI + prefill | enable/disable/approve from the `/mcp` panel — partially wired, verify it writes |
| Compaction/auto toggles | config | config file only | settings panel writing user-scope config |

**Design rule to adopt:** every user-scoped config key that is safe to change at runtime gets a TUI write path that round-trips through the engine (a `set_config`/`persist_setting` command with provenance, respecting the security rule that project scope can't set sensitive keys). Files remain the source of truth and the power-user path; the app is the default path. Config edits made in-app must be visible to `rw config check` with provenance `user (set via TUI)`.

## Ordering (per maintainer)

1. **Dynamic model catalog** (GAP-08-01, live discovery — not `models.toml`) — first.
2. Tool calls + `/` commands + surfaced projection errors (09-01, 09-02).
3. Bottom-bar meters (09-03 = GAP-02-01 + quota rendering).
4. Provider inventory + in-app auth (09-04), then the broader in-app settings surface (09-05).
5. Auto-routing via model-router (GAP-08-05) — **parked** until all of the above work.
