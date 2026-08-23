---
title: Session log
description: Durable append-only event envelope used for resume, replay, search, rewind, compaction, and redacted export.
sidebar:
  order: 6
---

Sessions are persisted as an append-only event stream. The durable envelope is
separate from the live client protocol so UI transport changes do not rewrite
history.

## Properties

- Each event has a schema version and stable identity fields.
- Replay reconstructs session behavior from the event stream.
- Resume, fork, rewind, checkpoints, search, and export operate on durable
  storage rather than TUI memory.
- Provider recordings preserve enough information for deterministic execution
  at the supported fidelity boundary.
- Export redaction happens before Markdown, HTML, or JSON leaves the engine.

## Canonical artifact

Download the [session-event envelope JSON Schema](/Rottweiler/generated/session/event-envelope.schema.json).
The schema, Rust schema-version constant, and maintainer format reference are
checked together in the repository.
