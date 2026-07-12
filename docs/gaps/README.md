# Gap Analysis — Rottweiler implementation review

Review date: 2026-07-12 (restored + extended later the same day; the original 01–08 files were deleted from disk — this folder was untracked — after an implementing agent consumed them; they are restored here with status updated).
Method: read the built crates (`crates/*`, `packages/tui`), built the workspace and the OpenTUI bundle, and exercised the `rw` binary end-to-end (print mode, line REPL, TUI launch, sessions, export, replay, doctor, stats, permissions, trust) against the user's live `openai_codex` config.

Each finding is tagged **[verified]** (reproduced at runtime), **[code]** (confirmed by reading the source), or **[design]** (the code does what it intends, but the intent is wrong or a spec requirement is unmet). Severity: **P0** breaks a headline capability; **P1** a real bug users will hit; **P2** rough edge.

> **Status note (same day, later):** commits `42935b9` (fail fast when engine startup exits), `400bd8b`/`400b8dd` (reject competing workspace engines), `ee12918` (preserve watchdog barrier on direct resume) landed after this review and appear to address parts of [01](01-tui-engine-startup.md). **Re-verify 01 against HEAD before working it.** The maintainer subsequently ran the TUI live and reported the interaction failures now captured in [09](09-tui-interaction-and-in-app-settings.md) — those are current as of HEAD.

## The one-paragraph verdict

The headless path is genuinely solid: `rw -p` runs a full tool-using turn, persists an event-sourced session, resumes, and the provider/subscription/credential machinery is real and careful. But **the interactive TUI experience and the context/cost instrumentation — the reasons the project exists — do not work for the maintainer's own configuration.** Engine/TUI startup was the first blocker (01, partially fixed since); with the TUI up, tool calls stall, `/` commands appear dead, meters read zero, and the model/provider surfaces are alias-shaped rather than catalog-shaped (08, 09). None of this is architectural rot — the bones match the design — but the integration seams were never exercised on a live subscription account end-to-end.

## Severity index

| # | Area | Top finding | Sev |
|---|------|-------------|-----|
| [01](01-tui-engine-startup.md) | TUI ↔ engine | TUI never connected; engine child stderr swallowed. **Partially addressed by post-review commits — re-verify** | **P0*** |
| [02](02-context-compaction.md) | Context/compaction | Subscription models have no context window → meters read 0, auto-compaction never triggers | **P0** |
| [03](03-permissions-modes.md) | Permissions | `auto-safe` denies *all* file writes → no non-yolo automation mode that can code | **P1** |
| [04](04-cli-ux.md) | CLI/UX | `rw replay` broken; session titling never runs; `models show` / `sessions list` missing; `doctor` ~23s | **P1** |
| [05](05-sandbox.md) | Sandbox | Seatbelt profile is `(allow default)` — deny-list, not allow-list | **P1** |
| [06](06-tools.md) | Tools | Mostly sound (edit fallback, SSRF guard, streaming present); gaps in checkpoint-for-bash surfacing | **P2** |
| [07](07-mcp-hooks-plugins.md) | MCP/hooks/plugins | Present and structured; deferred loading real; needs live conformance runs | **P2** |
| [08](08-models-providers-commands.md) | Model/provider/command pickers | Alias-centric, not catalog-centric — `/models` shows 5 roles, not the catalog; `/` menu built-ins only; provider list derived from aliases. Catalog must be **dynamic** (live discovery, never `models.toml`); auto-routing via model-router is **parked** | **P0** |
| [09](09-tui-interaction-and-in-app-settings.md) | TUI interaction + in-app settings | Tool calls stall (approval flow), `/` commands look dead (silently-swallowed projections + built-ins-only menu), bottom-bar meters dead, providers picker shows 1; settings are file-only — simple config must be manageable from the app | **P0** |

## The model/provider/command menus specifically

Rottweiler's model selection is **alias-centric** — `/models` can only show the five role aliases because `list_models` is built from `config.models.aliases`. opencode is **catalog-centric**: every `provider/model` across configured providers, fuzzy-searchable, grouped by provider. Two maintainer corrections are binding: (1) the catalog must be **dynamic** — queried live from providers at runtime, never from the static `models.toml` snapshot; (2) the desired **`auto` model** should reuse `../model-router` (which already implements cheapest-capable auto-routing *and* a live daily-refreshed catalog) — but auto is **parked** until the catalog and the TUI basics in [09](09-tui-interaction-and-in-app-settings.md) work.

## Cross-cutting theme

Every P0/P1 here shares a root cause: **the test suite is almost entirely replay/unit and never drove the live subscription path or the supervised TUI end-to-end.** `cargo test` is green and `bun test` is 132/132, yet the interactive product is unusable on the maintainer's machine. The highest-leverage investments: (a) surface every swallowed error (engine child stderr, opportunistic projection failures, rejected approvals); (b) one integration test that launches the real supervisor against a recorded engine and asserts the TUI reaches a ready, interactive state with populated pickers.

## Maintainer-set priority order

1. Dynamic model catalog (08) — live discovery, not files.
2. Tool calls + `/` commands + surfaced projection errors (09).
3. Bottom-bar meters (09/02 — subscription capabilities + quota rendering).
4. Provider inventory + in-app auth; then in-app settings surface (09).
5. Auto-routing via model-router (08) — parked until the above work.
