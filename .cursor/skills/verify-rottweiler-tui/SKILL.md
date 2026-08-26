---
name: verify-rottweiler-tui
description: Drive Rottweiler's OpenTUI terminal client through its production renderer and capture styled visual evidence when changing conversation, tool, approval, picker, composer, sidebar, or status UI.
---

# Verify the Rottweiler TUI

Use this skill after any user-visible change under `packages/tui`. The helper drives the real `RottweilerApp` through OpenTUI's native renderer at the product's 110 by 32 design size. It writes a plain terminal frame, terminal-native ANSI output, a direct raster PNG, and machine-readable assertions.

## Launch

Install the pinned TUI dependencies once with `cd packages/tui && bun install --frozen-lockfile`. A drive is short-lived and owns no server. Start one with `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh conversation /tmp/rottweiler-tui-evidence/<run-id>`. The printed path means the renderer completed and all scenario assertions passed. The helper destroys the renderer and Tree-sitter client before exit.

## Doctor

Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh doctor`. Require a Bun version, the pinned OpenTUI package version, and the visual helper path. If this fails, do not interpret screenshots from another checkout or a globally installed binary as proof.

## Drive

Read [the feature map](./features/README.md), then choose the matching command:

- `conversation` launches a populated live coding session through the production render tree.
- `command-palette` presses Ctrl+P through the renderer input path, then captures the resulting picker.
- `approval` launches a real pending-tool projection and captures the focused permission path.
- `tools` presses Ctrl+P, selects `View tools`, and captures the retained Tools workspace.
- `smoke` runs TypeScript checking, all golden screens, and the conversation capture.

Each run is isolated in its own native in-memory terminal. It does not connect to a provider, engine, repository mutation path, or another user's session.

To inspect typography, present the ANSI artifact in a real terminal emulator with `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh present /tmp/rottweiler-tui-evidence/<run-id>/conversation.ansi`. The presenter requires at least 110 columns by 32 rows. It never converts characters through SVG or another synthetic text layout engine.

## Evidence

Pass a unique directory under `/tmp/rottweiler-tui-evidence`. The helper preserves `<scenario>.txt`, `<scenario>.ansi`, `<scenario>.png`, and `<scenario>.json`. The text file proves the exact terminal cells. The ANSI file preserves the captured text runs, colors, backgrounds, and attributes for a real terminal emulator to draw. The PNG rasterizer draws complete style runs with a system monospace font. It never routes characters through SVG or positions glyphs individually. The JSON records actions plus text, position, and color assertions. The conversation scenario pins the supplied 110 by 32 design grid, including its column 73 divider and two-cell assistant indent. The Tools scenario pins its 74/1/35 primary-workspace split and rejects unsupported diagnostics, background-process, approval-actor, and matched-rule claims. A valid proof exercises `RottweilerApp`, not an HTML imitation or a manually assembled screenshot. For an input feature, capture both the action in JSON and the resulting screen. For engine side effects, pair this visual proof with the relevant process acceptance test.

## Cleanup

The helper tears down everything it starts in a `finally` block. Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh cleanup <evidence-dir>` to confirm there is no persistent process. Cleanup deliberately leaves the proof directory intact. Never kill processes by name.

## Helpers

`.cursor/skills/verify-rottweiler-tui/scripts/verify.sh` is the only entry point. Run it from any directory. `packages/tui/scripts/tui-visual-harness.ts` owns the typed scenarios and ANSI conversion. Use stable labels for ordinary scenarios. Use exact cells and colors only for the fixed 110 by 32 design contract.
