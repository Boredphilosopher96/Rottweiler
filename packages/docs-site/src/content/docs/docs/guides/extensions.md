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

RPC plugins communicate through bounded JSON-RPC frames on stdio. The host
validates the manifest before executing the process and checks every call
against declared capabilities.

Use the [TypeScript plugin SDK](../reference/plugin-sdk.md) to scaffold, validate,
develop, and package a plugin.
