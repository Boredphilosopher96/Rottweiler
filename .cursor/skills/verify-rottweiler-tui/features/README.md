# Rottweiler TUI verification map

This map is the maintained source for user-facing TUI checks. Run each recipe at 110 by 32 cells through the production OpenTUI render path.

## Baseline preconditions

- Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh doctor` from this checkout.
- Keep each proof in a new `/tmp/rottweiler-tui-evidence/<run-id>` directory.
- Do not connect the visual fixture to a provider, engine, or live user session.
- Use the pinned Bun and OpenTUI dependencies under `packages/tui`.

## Driving conventions

- Drive keyboard entry through the renderer input path.
- Treat the captured terminal cells as the source for visible text.
- Present the ANSI artifact in a real terminal for final typography evidence. Use the direct raster PNG for a portable visual review.
- Keep fixture projections protocol-valid and representative of real engine state.
- Pair mutations with process acceptance tests. The visual fixture is intentionally read-only.

## Proof and skip reporting

- Preserve the text, ANSI, PNG, and JSON from each run.
- Require every assertion in the JSON to report `passed: true`.
- Report a skipped user path by name. Do not call a nearby scenario equivalent.
- If doctor fails, record its output and stop. Evidence from another checkout is invalid.

## Feature entry contract

Each feature file names the user path, exact helper command, expected screen state, and traps that invalidate proof.

## Features

- [Conversation](./conversation.md) covers user and assistant hierarchy, reasoning, tool rows, side context, composer, and status.
- [Command palette](./command-palette.md) covers the Ctrl+P entry path and command discovery.
- [Approvals](./approvals.md) covers pending tool permission context and available choices.
- [Tools workspace](./tools.md) covers retained tool activity, truthful turn totals, queue semantics, and complete-output discovery.
