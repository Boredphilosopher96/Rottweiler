# Archived implementation review — July 2026

This directory preserves the gap analysis performed on 2026-07-12. It is a
historical engineering record, not current product documentation or an active
issue tracker.

The review read the built crates (`crates/*`, `packages/tui`), built the
workspace and OpenTUI bundle, and exercised `rw` end-to-end (print mode, line
REPL, TUI launch, sessions, export, replay, doctor, stats, permissions, and
trust) against the reviewer's then-current `openai_codex` configuration.

Each finding is tagged **[verified]** (reproduced at runtime), **[code]** (confirmed by reading the source), or **[design]** (the code does what it intends, but the intent is wrong or a spec requirement is unmet). Severity: **P0** breaks a headline capability; **P1** a real bug users will hit; **P2** rough edge.

> **Archive notice:** these files preserve the evidence and remediation record
> from the July 12 review. Their “current state” sections describe the product
> at the time each finding was opened and must not be used as current
> documentation. Start with the repository `README.md` and
> `docs/01-FEATURES.md`; use the architecture, extensibility, security, and
> verification documents for their respective contracts.

## Recorded outcome

At the close of this review, the product ran as one supervised application and
the headless and interactive paths shared the same durable engine. Startup,
approvals, commands, meters, live provider/model catalogs, in-app auth, safe
settings, themes, permissions, MCP management, and structured text/image
attachments had functional acceptance coverage. Provider sign-in was
independent from model selection: credentials remained outside replayable
protocol state, catalog refresh was separately retryable, and activation did
not depend on unrelated aliases. The model-router `auto` integration remained
explicitly parked by the maintainer.

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

## Cross-cutting theme

The original cross-cutting failure was missing production-composition acceptance. The suite now launches the real supervisor/TUI boundary, verifies authenticated readiness and approval round trips, exercises process recovery/replay, and tests typed provider, permission, and MCP workflows alongside unit and replay coverage.
