---
title: Architecture
description: How the Rust engine, supervised clients, providers, tools, storage, and extension hosts form one Rottweiler application.
sidebar:
  order: 1
---

Rottweiler is one product with deliberately separate processes.

```text
rw supervisor
├── Rust session engine
│   ├── provider adapters and router
│   ├── context and compaction
│   ├── tools, permissions, sandbox, and trust
│   └── durable event storage and replay
├── compiled OpenTUI client
├── WASM extension host
└── TypeScript plugin host
```

Only `rw` is public. Sibling executables are bundle members supervised by the
application.

## Why the engine is headless

The terminal UI, one-shot prompts, remote clients, tests, and MCP server all use
the same session behavior. A UI cannot silently acquire its own routing,
permission, persistence, or extension rules.

## Provider boundary

Providers adapt external wire formats into one internal message and event
model. Bounded catalog information can cross into clients; credentials,
endpoints, proxy state, and provider implementation details remain in Rust.

## Extension boundary

Declarative extensions are data until trusted. Executable hooks, MCP servers,
WASM components, and RPC plugins cross explicit capability and approval
boundaries. Built-in registrations use the same registries where the product
claims extension parity.
