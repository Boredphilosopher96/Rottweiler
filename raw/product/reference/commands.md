The engine owns one catalog of built-in commands. Slash completion, the Ctrl+P
command palette, and `/help` all show this catalog, grouped by section in the
order below. Type `/` in an empty composer for completion; `?` on an empty
composer opens `/help`.

Aliases resolve to the canonical command before it runs. Keys are the default
standard bindings; `/help` shows your effective bindings.

## Conversation

| Command | Aliases | Arguments | Key | Purpose |
|---|---|---|---|---|
| `/new` | | | Ctrl+N | Start a clean conversation. |
| `/resume` | `/sessions` | | Ctrl+S | Resume, rename, or export a session. |
| `/rewind` | | `[turn]` | | Restore or fork from a completed turn. |
| `/compact` | | `[instructions]` | | Summarize older context to free space. |
| `/queue` | | | | Review, remove, or clear queued messages. Listed only while messages are queued. |

## Models and agents

| Command | Aliases | Arguments | Key | Purpose |
|---|---|---|---|---|
| `/model` | `/models`, `/providers` | | Alt+M | Choose a model or connect a provider. |
| `/mode` | | `[discuss\|plan\|execute]` | Shift+Tab | Switch between discuss, plan, and execute. |
| `/agents` | | | Ctrl+G | Inspect, continue, or stop child agents. Listed only once the session has child agents. |

## Context and usage

| Command | Aliases | Arguments | Key | Purpose |
|---|---|---|---|---|
| `/context` | | `[pin\|evict <item-id>]` | | Inspect, pin, or evict context items. |
| `/usage` | `/cost`, `/budget` | | | Tokens, cost, and budget limits. |

## Workspace

| Command | Aliases | Arguments | Key | Purpose |
|---|---|---|---|---|
| `/review` | | | Ctrl+R | Accept or revert this session's file changes. |
| `/dirs` | `/add-dir` | `[path]` | | List workspace roots or add one. |
| `/mcp` | | `[status\|enable\|disable\|approve\|prompt]` | | Manage servers, prompts, and panels. |
| `/init` | | `[--deep]` | | Write AGENTS.md for this repository. |
| `/memory` | | `[list\|read <id>\|write <text>\|clear]` | | Read or update private project memory. |

## Safety

| Command | Aliases | Arguments | Key | Purpose |
|---|---|---|---|---|
| `/permissions` | `/trust` | `[mode\|approvals\|add\|remove\|trust\|…]` | | Approval policy, rules, and folder trust. |

## Settings and help

| Command | Aliases | Arguments | Key | Purpose |
|---|---|---|---|---|
| `/settings` | | | | Change saved user settings. |
| `/skills` | | | | Skills, commands, and agents with their load status. |
| `/theme` | | | | Preview and choose an interface theme. |
| `/help` | `/keys` | | `?` | Commands and keyboard shortcuts. |
| `/errors` | | | | Recent failures and recovery steps. Listed only when errors exist. |
| `/exit` | | | | Close Rottweiler. |

## Extension commands

Skills, user and project commands, plugin commands, `/workflow`, and MCP
prompts follow the built-in sections. Each is labelled with its source, such as
`skill`, `project command`, or `mcp · github`. See
[Extensions](../guides/extensions.md).

## Agents

While a session has child agents, an Agents strip above the composer lists
them with their state. Ctrl+G or `/agents` opens the Agents screen:

| Action | Effect |
|---|---|
| View | Watch the child's transcript over the parent view; the parent keeps running and Escape returns. |
| Message | Send a follow-up to a child that is not running; it keeps the context of its earlier work. |
| Stop | Interrupt a running child; its partial report is delivered to the parent. |
| Background | For the child the parent is waiting on: the parent continues and the result arrives when the child finishes. |
| Close | Release the child and its worktree; its result stays in the parent transcript. |

Ctrl+B moves the running foreground child to the background without opening
the screen. A child view is read-only unless the child is waiting for a typed
answer.
