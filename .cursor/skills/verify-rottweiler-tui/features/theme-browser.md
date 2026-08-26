# Theme Browser

The Theme Browser is a retained list/detail surface for previewing and applying the resolved theme catalog. The list owns filtering, stable selection, scrolling, and five semantic swatches. The detail pane renders live samples from the selected theme's 52 resolved roles.

## Sub-features

- `theme-open` enters through the built-in `/theme` completion.
- `theme-catalog` preserves the resolved System, built-in, and custom catalog order.
- `theme-filter` fuzzily filters names while retaining a visible stable selection.
- `theme-preview` rebuilds the complete surface immediately without losing query, selection, viewport, draft, or focus.
- `theme-apply` persists the exact selected name through `set_setting ui.theme`.
- `theme-cancel` restores the pre-preview theme and emits no setting command.
- `theme-system` follows the terminal's current dark or light mode.
- `theme-responsive` uses full-primary 34/1/75 regions at 110 by 32, a full-width list through 99 columns, and a 64-cell detail pane at the 100-column split threshold.

## Driving it with the visual harness

Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh theme-browser /tmp/rottweiler-tui-evidence/<run-id>`.

The harness types `/the` through the production composer, activates the `/theme` completion with Enter, and captures the production renderer at 110 by 32. Require all four artifacts:

- `theme-browser.txt` proves exact terminal cells and intact text.
- `theme-browser.ansi` preserves terminal-native color runs.
- `theme-browser.png` is the direct raster proof.
- `theme-browser.json` records the slash entry path and exact layout, color, occlusion, and spacing assertions.

The JSON must pin the surface from column 0 and row 0 through the complete 27-row primary area, with the divider at column 34, the right container at column 35, and detail content at column 36. It also pins five selected-theme swatches, visible role samples, complete 110-cell rows, full occlusion of the prior conversation/context surface, and a focused theme query. Every assertion must pass and the presentation contract must say `characterSvg: false`.

## Focused behavior proof

- Pure model behavior: `cd packages/tui && bun test test/theme-browser.test.ts`.
- Retained component and threshold behavior: `cd packages/tui && bun test test/components.test.ts -t "34-cell list"`.
- Preview, apply, cancel, rejection, System mode, Vim focus, and 64 by 14 behavior: `cd packages/tui && bun test test/app.test.ts -t "theme"`.
- Deterministic production artifacts: `cd packages/tui && bun test test/visual-harness.test.ts -t "production theme browser"`.

## Gotchas

- Opening `themeBrowser` directly does not prove the `/theme` user path.
- A selected row previews immediately; it does not persist until Enter.
- Escape after a preview must restore the exact theme that was active before opening.
- The narrow surface intentionally omits the detail pane; it must not squeeze or clip the semantic preview beside the list.
- The proof must not create SVG text or position characters individually.
