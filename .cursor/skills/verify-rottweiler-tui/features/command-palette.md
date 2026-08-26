# Command palette

The command palette is a split list/detail surface for keyboard-first command discovery. It keeps command descriptions in one full-width detail pane instead of squeezing every description into the action list.

## Sub-features

- `palette-open` opens command discovery with Ctrl+P.
- `palette-catalog` shows built-in and engine commands with derived source counts.
- `palette-filter` accepts a query in the focused input.
- `palette-detail` updates only from the selected action.
- `palette-refresh` preserves query, stable selection, and viewport while the live catalog changes.
- `palette-retry` retries a failed live catalog with Ctrl+R while built-in actions remain available.
- `palette-close` returns focus to the composer with Escape.

## How to get to it (user POV)

- Press Ctrl+P anywhere in the main session.
- Type a command name to filter the list.
- Press Enter to choose, Ctrl+R to retry a failed live catalog, or Escape to close.

## Driving it with the visual harness

Preconditions:

- Doctor reports the pinned local renderer.
- The fixture command catalog contains context and review actions.
- The production terminal is 110 columns by 32 rows.

- **Open and filter.** Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh command-palette /tmp/rottweiler-tui-evidence/<run-id>`. The helper presses Ctrl+P and types `context` through the same renderer input path as a user.
- **Check terminal cells.** Read `command-palette.txt`. Confirm `COMMAND PALETTE`, the intact query, `Compact context`, `Manage context`, and the selected description in the detail pane.
- **Check exact layout.** Read `command-palette.json`. It asserts the 108 by 25 modal at column 1 and row 2, the 52/1/51 pane split, the fixed divider, full 110-cell rows, derived result/source counts, selected-only description, and intended text tones.
- **Check renderer evidence.** Every JSON assertion must report `passed: true`. The presentation contract must say `characterSvg: false`.
- **Proof.** Inspect the direct-raster `command-palette.png`. Present `command-palette.ansi` in a real terminal when terminal-profile typography matters. Confirm the right pane is readable and unclipped.

## Gotchas

- Calling `openCommandPicker()` directly is not proof of the keyboard entry path.
- The fixture has no remote command refresh. Verify loading, failure/retry, truncation, and refresh retention in focused application tests.
- Do not infer focus from appearance alone. The JSON action and the input-path drive are both required.
- The compact slash autocomplete remains a separate composer-anchored picker. This proof covers Ctrl+P only.
- The harness writes PNG, ANSI, TXT, and JSON. It must not write SVG output.
