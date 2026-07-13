# Gap Analysis — Rottweiler implementation review

Review date: 2026-07-12 (restored + extended later the same day; the original 01–08 files were deleted from disk — this folder was untracked — after an implementing agent consumed them; they are restored here with status updated).
Method: read the built crates (`crates/*`, `packages/tui`), built the workspace and the OpenTUI bundle, and exercised the `rw` binary end-to-end (print mode, line REPL, TUI launch, sessions, export, replay, doctor, stats, permissions, trust) against the user's live `openai_codex` config.

Each finding is tagged **[verified]** (reproduced at runtime), **[code]** (confirmed by reading the source), or **[design]** (the code does what it intends, but the intent is wrong or a spec requirement is unmet). Severity: **P0** breaks a headline capability; **P1** a real bug users will hit; **P2** rough edge.

> **Resolution note (2026-07-12):** every non-parked finding in 01–09 is implemented and re-verified against the functional suite. GAP-08-05 (`auto` via `../model-router`) remains intentionally parked by maintainer direction; it is not silently treated as complete.

## The one-paragraph verdict

The reviewed product now ships as one supervised application and the headless and interactive paths share the same durable engine. Startup, approvals, commands, meters, live provider/model catalogs, in-app auth, safe settings, themes, permissions, MCP management, and structured text/image attachments have functional acceptance coverage. Provider sign-in is independent from model selection: credentials remain outside replayable protocol state, catalog refresh is separately retryable, and activation never depends on unrelated aliases. Only the maintainer-parked model-router `auto` integration remains open.

## Severity index

| # | Area | Top finding | Sev |
|---|------|-------------|-----|
| [01](01-tui-engine-startup.md) | TUI ↔ engine | Resolved and supervised end-to-end | Closed |
| [02](02-context-compaction.md) | Context/compaction | Resolved with explicit unknown capacity | Closed |
| [03](03-permissions-modes.md) | Permissions | Resolved with reversible workspace writes in auto-safe | Closed |
| [04](04-cli-ux.md) | CLI/UX | Resolved and runtime-verified | Closed |
| [05](05-sandbox.md) | Sandbox | Resolved within the recorded macOS/Linux security contracts | Closed |
| [06](06-tools.md) | Tools | Runtime acceptances complete | Closed |
| [07](07-mcp-hooks-plugins.md) | MCP/hooks/plugins | Live conformance acceptances complete | Closed |
| [08](08-models-providers-commands.md) | Model/provider/command pickers | Dynamic catalog and typed switching resolved; auto-router remains explicitly parked | Closed / parked |
| [09](09-tui-interaction-and-in-app-settings.md) | TUI interaction + in-app settings | Interactive and typed settings/auth surfaces resolved | Closed |

## The model/provider/command menus specifically

Rottweiler's model selection is **alias-centric** — `/models` can only show the five role aliases because `list_models` is built from `config.models.aliases`. opencode is **catalog-centric**: every `provider/model` across configured providers, fuzzy-searchable, grouped by provider. Two maintainer corrections are binding: (1) the catalog must be **dynamic** — queried live from providers at runtime, never from the static `models.toml` snapshot; (2) the desired **`auto` model** should reuse `../model-router` (which already implements cheapest-capable auto-routing *and* a live daily-refreshed catalog) — but auto is **parked** until the catalog and the TUI basics in [09](09-tui-interaction-and-in-app-settings.md) work.

## Cross-cutting theme

The original cross-cutting failure was missing production-composition acceptance. The suite now launches the real supervisor/TUI boundary, verifies authenticated readiness and approval round trips, exercises process recovery/replay, and tests typed provider, permission, and MCP workflows alongside unit and replay coverage.

## Maintainer-set priority order

1. Dynamic model catalog (08) — live discovery, not files.
2. Tool calls + `/` commands + surfaced projection errors (09).
3. Bottom-bar meters (09/02 — subscription capabilities + quota rendering).
4. Provider inventory + in-app auth; then in-app settings surface (09).
5. Auto-routing via model-router (08) — parked until the above work.
