# 08 — Model / provider / command selection (the opencode-parity gap)

The maintainer's ask: models, the `/` command menu, and provider selection should work **like opencode**. Today they don't — and the reason isn't only that the TUI struggled to connect (01). Even with a live TUI, the *data* behind these pickers is the wrong shape. opencode is **catalog-centric** (pick a concrete `provider/model`); Rottweiler is **alias-centric** (pick a role). That is the root divergence.

## GAP-08-01 — `list_models` returns 5 role aliases, not a model catalog — **P0 [code]**

**Resolved (2026-07-12).** The shared session catalog now discovers configured providers live with bounded concurrency and timeouts, caches briefly with explicit refresh, enriches capabilities/pricing separately, and returns concrete provider-qualified models plus aliases, provider state, availability, and current markers. CLI and TUI read the same source; one provider failure remains visible without blocking healthy routes.

`ModelDescriptor` is keyed by **alias** (`rw-types/src/protocol.rs:206`: `alias`, `providers`, `capabilities`), and the real host builds it by iterating `config.models.aliases` (`rw-cli/src/host_runtime.rs:1614`). So `/models` can only ever show `big`, `fast`, `plan`, `compact`, `title` — five rows — regardless of how many models exist.

opencode enumerates **every model of every configured provider** (`opencode/src/provider/provider.ts:1003`) and its `/models` picker lists all of them as `provider/model`, fuzzy-searchable, grouped by provider.

The protocol can already express a concrete switch: the TUI's `switch_model` carries `{ model, provider }` (`packages/tui/src/app.ts:1023`). The missing half is a listing that offers concrete models.

### The catalog is DYNAMIC — not a static file (maintainer directive, twice stated)

Do **not** source the picker from `~/.rottweiler/models.toml`. That file is a stale snapshot written only by manual `rw models refresh` (one-shot pull of `models.dev/api.json`, `rw-providers/src/models_dev.rs:23`) — a caps/pricing *enrichment* layer, never the source of truth for "what can I pick right now." The available-model list must be **queried live from each configured, authenticated provider at runtime**:

- OpenAI / OpenAI-compatible: `GET /v1/models`.
- Anthropic: the models list endpoint.
- GitHub Copilot: its `/models` discovery — which Rottweiler **already implements** (`provider_factory.rs` copilot path); generalize that pattern to every provider instead of leaving it Copilot-only.
- Enrich each discovered id with capabilities/pricing from the refreshable models.dev data when available — but the *list* comes from the provider, not the file.

**Fix (opencode-shaped and dynamic).**
- `list_models` calls per-provider `discover_models()` on each authenticated provider (concurrent, briefly cached per session, refreshable), unions results, returns `{ id: "provider/model", display_name, provider, capabilities }` with a "current" marker and which alias(es) resolve to it.
- Unreachable providers degrade gracefully (last-known or an "unavailable" row) — never block the picker.
- Aliases remain a small separate section at the top ("fast → gpt-5.4-mini"); the body is the live catalog.
- CLI (GAP-04-02): `rw models list` / `show <id>` read the same live discovery, `--refresh` busts the cache.

## GAP-08-02 — Provider picker is derived from alias references, not configured providers — **P1 [code, user-confirmed]**

**Resolved (2026-07-12).** The provider picker consumes the engine's bounded provider inventory, including configured-but-unaliased and supported setup targets, and exposes in-app configure/authenticate/recover actions.

**Activation follow-up resolved (2026-07-12).** Credential persistence now completes sign-in independently from catalog availability. Connecting a provider stages its runtime and refreshes its live catalog without replacing the selected model; an explicit later model selection commits the staged runtime transactionally, and previous provider generations remain switchable. The TUI reports “signed in, models unavailable” with a catalog retry instead of falsely reporting login failure.

The `/providers` picker counts provider names across alias descriptors (`app.ts:1032`). The maintainer's aliases are all `openai/*` → exactly one provider shown, which they hit live ("providers option only shows 1"). A configured-but-unaliased provider is invisible; the provider list is a side-effect of alias config. See 09/GAP-09-04 for the full design fix (provider inventory + in-app auth).

## GAP-08-03 — The `/` command menu shows built-in commands only — **P1 [code]**

**Resolved (2026-07-12).** Command discovery merges built-in, project, user, skill, and plugin sources under the trust/discovery rules and projects source-tagged descriptors into slash autocomplete and the full command palette.

`command_descriptors()` (`rw-cli/src/host_runtime.rs:1602`) returns only `builtin_command_registry()` — never custom commands from `.agents/commands/` / `.rottweiler/commands/`, skills, or plugin commands. **Fix:** merge all sources (respecting trust + ADR-014 discovery order), tagging each with its origin so the palette can group Built-in / Project / User / Plugin.

## GAP-08-04 — `switch_model` has no concrete-model source in the UI — **P1 [code]**

**Resolved (2026-07-12).** Concrete catalog rows dispatch exact provider/model selections, the host durably persists the accepted project preference in lifecycle order, and status/current markers show the resolved concrete route before and after a turn without double qualification.

Because the list is alias-only, `switch_model { model, provider }` gets an alias string. Once GAP-08-01 lands, verify switching to a concrete `provider/model` re-targets the session and that the status line shows the **resolved concrete model**, not the alias (opencode shows the concrete model).

## GAP-08-05 — "auto" model via the existing `../model-router` — **PARKED per maintainer (2026-07-12)**

Parked until the dynamic catalog (08-01) and the TUI basics (09) work. Recording the design so it isn't lost:

The maintainer wants **auto** = cheapest model capable of the current turn, and it must **reuse their `../model-router` project**, not be reimplemented. Verified in `../model-router/src`: `router.ts:31` `isAutoModel()` accepts `"auto"`/`"model-router-auto"`; auto drops the tier ceiling and `route()` picks the cheapest (model, upstream) pair meeting the classified tier + capability floors — plus classification, cache-aware stickiness, escalation-when-stuck, budgets, circuit-breaker failover. It also maintains a **live daily-refreshed catalog** (`registry.ts` + `pricefeed.ts`) exposed at `GET /api/models` / `GET /v1/models`.

Integration (when unparked): configure a provider whose `base_url` is the running model-router; add an `auto` model entry that sends `model: "auto"`; surface the routed model from `x-router-*` headers in the status line; optionally source `list_models` from `/api/models` when a router is configured. Connect-only first; supervised launch later; absence fails open to normal alias routing; never embedded in the Rust core.

## Why this reads as "nothing works"

Stacked on the startup issues (01), zeroed meters (02), and the interaction failures (09), these surfaces would look broken even if perfectly wired. Fix order per maintainer: dynamic catalog → tool calls & `/` commands → meters → provider inventory/in-app settings → auto.
