# Command palette

The command palette lets a user open command discovery from the keyboard, filter available actions, and choose a command without losing the session behind it.

## Sub-features

- `palette-open` opens command discovery with Ctrl+P.
- `palette-catalog` shows engine and built-in commands.
- `palette-filter` accepts a query in the focused input.
- `palette-close` returns focus to the composer with Escape.

## How to get to it (user POV)

- Press Ctrl+P anywhere in the main session.
- Type a command name to filter the list.
- Press Enter to choose or Escape to close.

## Driving it with the visual harness

Preconditions:

- Doctor reports the pinned local renderer.
- The fixture command catalog contains context and review actions.

- **Open palette.** Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh command-palette /tmp/rottweiler-tui-evidence/<run-id>`. The helper presses Ctrl+P through the same renderer input path as a user.
- **Check result.** Read `command-palette.txt`. It contains the `Command palette` title plus `Compact context` and `Review changes`.
- **Check action evidence.** Read `command-palette.json`. Its action states that Ctrl+P passed through renderer input and every assertion reports `passed: true`.
- **Proof.** Inspect `command-palette.png`, then present `command-palette.ansi` in a real terminal when terminal-profile typography matters. Confirm the picker is legible over the retained session.

## Gotchas

- Calling `openCommandPicker()` directly is not proof of the keyboard entry path.
- The fixture has no remote command refresh. Verify remote catalog behavior in its focused application test.
- Do not infer focus from appearance alone. The JSON action and the input-path drive are both required.
