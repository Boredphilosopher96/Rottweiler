---
title: Extensions
description: Choose commands, skills, agents, workflows, modes, hooks, MCP, WASM, or RPC plugins without widening the engine boundary.
sidebar:
  order: 3
---

Choose the smallest extension mechanism that fits the job.

| Need | Mechanism |
|---|---|
| Reusable prompt | Command |
| Instructions and resources | Skill |
| Specialized delegated role | Agent |
| Multi-step orchestration | Workflow |
| Interaction and tool policy | Mode |
| Lifecycle side effect | Hook |
| External tool/service | MCP |
| Capability-scoped in-process hook | WASM component |
| Tools, commands, hooks, events, or provider dialect | RPC plugin |

Project extension files use `.agents/` directories and participate in the
trust inventory. Plugin and MCP execution also require their own fingerprint-
bound approvals.

## Skills

A skill is a directory with a `SKILL.md` file. Rottweiler reads skills from
`.agents/skills`, `.rottweiler/skills`, and `.claude/skills` in the project and
in your home directory, in that order; the first skill with a given name wins.
Project skills load only after you trust the project.

Existing Claude Code skills and commands work in place: there is nothing to
import. Skill directories and `SKILL.md` files may be symbolic links, for
example into a shared checkout. A link in your home directory may point
anywhere you own; a project link must stay inside the project or your home
directory.

```markdown
---
description: |
  Review a pull request for correctness and safety.
  Use when asked to "review this PR".
allowed-tools:
  - Bash
  - Read
---
Review instructions…
```

The directory name is the skill name. `description` is required; YAML block
scalars (`|`, `>`) and keys Rottweiler does not use (such as `hooks:`) are fine.

Run a skill yourself with `/<name> [arguments]`. The model sees every skill's
name and description and loads a matching skill with the built-in `skill` tool.
Either way it receives the `SKILL.md` instructions, the skill's directory, and a
list of bundled files, which it opens on demand. Large bundles never prevent a
skill from loading.

`allowed-tools` limits the tools available while your `/<name>` invocation runs.
Claude Code tool names such as `Read`, `AskUserQuestion`, or `Bash(git status:*)`
are translated; names Rottweiler does not have are ignored.

In the TUI, `/skills` lists every discovered skill, command, and agent with its
source and load status. `rw doctor` lists every skill, command, or agent that
was skipped, hidden by a higher-precedence one of the same name, or waiting for
project trust, with the reason.

RPC plugins communicate through bounded JSON-RPC frames on stdio. The host
validates the manifest before executing the process and checks every call
against declared capabilities.

Use the [TypeScript plugin SDK](../reference/plugin-sdk.md) to scaffold, validate,
develop, and package a plugin.

Run `rw plugin check <path> --allow-exec` before live development. It validates
package identity and runs the plugin's declared typecheck and test scripts
without attaching it to an engine.
