# Approvals

Approvals keep the triggering conversation visible while naming the tool, rationale, command, and choices before any capability crosses a permission boundary.

## Sub-features

- `approval-context` retains the request and reasoning that produced the tool call.
- `approval-command` shows the exact terminal command.
- `approval-choices` offers allow-once, broader safe choices when available, and deny.
- `approval-focus` routes safety-panel keys to the approval instead of the composer.

## How to get to it (user POV)

- Ask Rottweiler to perform an operation that requires approval.
- Read the command and rationale.
- Choose an allowed scope or deny the request.

## Driving it with the visual harness

Preconditions:

- Doctor reports the pinned local renderer.
- The scenario uses a pending terminal-command tool projection.

- **Launch approval.** Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh approval /tmp/rottweiler-tui-evidence/<run-id>`. No command executes; the projection stops at the real approval UI.
- **Check context.** Read `approval.txt`. It contains `Permission required`, `Terminal command`, and `Allow once` while the conversation remains visible.
- **Check proof.** Read `approval.json` and require every assertion to have `passed: true`.
- **Inspect style.** Inspect `approval.png`, then present `approval.ansi` in a real terminal when terminal-profile typography matters. Warning color marks the permission boundary and the transcript remains behind the decision UI.

## Gotchas

- This visual scenario must not approve or execute the command.
- Approval round-trip behavior belongs to `approval-roundtrip-worker.ts` and the process acceptance suite.
- A banner saying approval is pending is not enough. The exact command and choices must be visible.
