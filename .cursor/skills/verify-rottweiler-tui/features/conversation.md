# Conversation

Conversation presents the user's request, live reasoning, response Markdown, tool activity, tasks, changed files, child agents, composer, and session identity in one terminal.

## Sub-features

- `conversation-hierarchy` distinguishes user, assistant, reasoning, and tools by gutters and color.
- `conversation-context` shows tasks, changed files, and active services without covering the transcript.
- `conversation-compose` keeps the input and one-row session status visible.
- `conversation-agents` shows live child-agent activity.

## How to get to it (user POV)

- Start Rottweiler in a repository and submit a coding request.
- Watch a turn while the assistant reasons, uses tools, updates tasks, and runs child agents.

## Driving it with the visual harness

Preconditions:

- Doctor reports the pinned local renderer.
- The evidence directory does not contain an earlier run.

- **Launch conversation.** Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh conversation /tmp/rottweiler-tui-evidence/<run-id>`. The helper launches the real app with a live protocol-shaped session.
- **Check hierarchy.** Read `conversation.txt`. The user gutter starts at column 0, the assistant marker starts at column 0, and assistant prose and tools start at column 2.
- **Check context.** The divider stays at column 73. `AGENTS`, `TASKS`, `CHANGED`, `SESSION`, and `SERVICES` share the same right-panel baseline.
- **Check style.** Inspect `conversation.png`, then present `conversation.ansi` in a real terminal when terminal-profile typography matters. The user gutter uses the primary color, reasoning uses the subtle gutter and muted text, tool names use the secondary color, and the mode has an inverted primary pill.
- **Proof.** Read `conversation.json` and require every assertion to have `passed: true`. The assertions cover exact cells, reasoning color on every visible line, concealed Markdown, and the status/composer inset.

## Gotchas

- The ANSI and PNG artifacts must come from the same run as the text and JSON.
- A browser render of the design document is a reference, not product proof.
- A snapshot update without inspecting the captured screen does not prove visual quality.
- Narrow terminals intentionally hide the context panel. Use 110 by 32 for design comparison.
