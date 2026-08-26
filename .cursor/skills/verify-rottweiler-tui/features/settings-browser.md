# Settings Browser

The Settings Browser is a retained full-primary list/detail surface built only from engine-provided setting descriptors. It groups populated settings, keeps current values and provenance in the detail pane, and delegates value selection to bounded choice pickers.

## Sub-features

- `settings-open` enters through the built-in `/settings` completion.
- `settings-sections` groups only populated descriptor-backed sections and preserves unknown settings under Other.
- `settings-choices` emits an exact immediate `set_setting` command from the selected descriptor choice.
- `settings-theme` hands theme selection to the existing Theme Browser.
- `settings-budget` hands budget editing to the existing Budget limits flow.
- `settings-read-only` keeps project default model visible without falsely routing to the active-session model switcher.
- `settings-responsive` uses 30/1/79 regions at 110 by 32 and removes the divider and detail pane below 90 columns.
- `settings-request-state` preserves cached values during refresh and rejection while keeping retries correlated.

## Driving it with the visual harness

Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh settings-browser /tmp/rottweiler-tui-evidence/<run-id>`.

The harness types `/sett` through the production composer, activates `/settings`, and captures both 110 by 32 and 72 by 18. Require eight artifacts:

- `settings-browser.{txt,ansi,png,json}` for the wide split surface.
- `settings-browser-narrow.{txt,ansi,png,json}` for the narrow single-pane surface.

Every JSON assertion must pass and both presentation contracts must say `characterSvg: false`. The wide proof pins the divider at column 30 and detail content at column 32. Both proofs reject transcript bleed, per-character spacing, and unsupported `save`, `reset`, `discard`, `diff`, changed-key, or config-path claims.

## Focused behavior proof

- Pure model behavior: `cd packages/tui && bun test test/settings-browser.test.ts`.
- Empty-state ownership and retained layout: `cd packages/tui && bun test test/components.test.ts -t "presentation-owned empty copy"`.
- Correlation behavior: `cd packages/tui && bun test test/projection-requests.test.ts`.
- Choice dispatch, rejection, retry, Vim focus, and responsive layout: `cd packages/tui && bun test test/app.test.ts -t "Settings"`.
- Deterministic artifacts: `cd packages/tui && bun test test/visual-harness.test.ts -t "production settings browser"`.

## Gotchas

- `project.models.default` is not the active-session model. Do not route it to `switch_model`.
- The protocol has no staged draft, unset/reset operation, pending-key count, multi-key save, or universal config path.
- Do not render empty categories or invent values that were not returned by `list_settings`.
- The narrow layout intentionally removes the detail pane instead of squeezing or clipping it.
- The proof must not create SVG text or position characters individually.
