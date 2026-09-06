---
title: Ownership model
description: The one-owner rule that keeps Rottweiler protocols, configuration, releases, sessions, and documentation coherent.
sidebar:
  order: 2
---

Every piece of data and every feature has one semantic owner. Other layers
consume projections; they do not redefine the fact.

| Concern | Owner |
|---|---|
| Provider-neutral session behavior | Rust engine |
| Visual interaction and rendering | OpenTUI client |
| Config fields and typed defaults | `rw-types` |
| Config discovery and precedence | `rw-store` |
| Plugin API values and limits | `rw-plugin-protocol` |
| Release archive membership | `contracts/release-contract.json` |
| Signed published targets | signed update channel metadata |
| Durable session envelope | session schema owner and checked projections |
| Public documentation experience | `packages/docs-site` |
| Signed update origin subtree | release workflow |

Generated schemas, fixtures, SDK declarations, raw Markdown, agent indexes, and
HTML are projections. A projection has a drift check or is created during the
build; it does not become a second hand-maintained authority.

Implementations and callers use the owned contract directly. Boundary validators
reject inputs outside it.
