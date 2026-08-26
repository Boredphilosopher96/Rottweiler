# Tools workspace

The Tools workspace is a mounted primary view for retained tool and foreground-shell activity. It owns independent scrolling, row selection, and folding while reusing the existing complete-output viewer.

## How to get to it

- Press Ctrl+P.
- Choose `View tools`.
- Use the existing block-navigation bindings to select and fold rows.
- Choose a retained-output marker to open the complete output viewer.

## Visual harness

Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh tools /tmp/rottweiler-tui-evidence/<run-id>`.

The harness enters through Ctrl+P and Enter, then captures the production renderer at 110 by 32. Require all of these artifacts:

- `tools.txt` for exact terminal cells.
- `tools.ansi` for terminal-native styled runs.
- `tools.png` for the direct raster.
- `tools.json` for the driven actions and assertions.

The proof pins the activity pane to columns 0 through 73, the divider to column 74, and rail content to columns 75 through 109. It also checks full row widths, complete text runs, double-Escape interrupt copy, outcome colors, queue wording, and the absence of unsupported automatic diagnostics, live background state, approval actor, or matched-rule claims.

## Gotchas

- Opening the component directly does not prove the palette entry path.
- A `diagnostics` tool result is an ordinary retained tool row. It is not evidence for an automatic diagnostics lifecycle.
- Prior `background_status` output is not evidence that a process is currently running.
- Running-turn tokens and cost remain absent until the protocol supplies final turn accounting.
- The harness writes no SVG.
