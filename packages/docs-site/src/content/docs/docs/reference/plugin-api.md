---
title: Plugin API
description: Language-neutral JSON-RPC contract for Rottweiler tools, commands, hooks, events, providers, and host-mediated services.
sidebar:
  order: 4
---

The Plugin API uses newline-framed JSON-RPC over standard input and output.
The protocol crate owns method names, limits, validation, manifest
normalization, fingerprints, schemas, fixtures, and TypeScript projections.

## Lifecycle

1. The host validates the manifest and approval fingerprint.
2. The host starts the executable with bounded stdio.
3. Host and plugin exchange initialization identities.
4. The plugin serves only declared contributions and capabilities.
5. The host requests shutdown and enforces bounded process cleanup.

## Contributions

Plugins can provide tools, commands, hooks, event subscriptions, and provider
adapters. Provider adapters publish a bounded model catalog and can request
host-mediated authenticated HTTP using an approved credential reference. The
plugin never receives the credential value.

## Canonical artifacts

- [JSON Schema](/Rottweiler/generated/plugin/schema.json)
- [Wire example](/Rottweiler/generated/plugin/wire-example.json)
- [Frontmatter-free reference Markdown](/Rottweiler/raw/product/reference/plugin-api.md)

These files are copied from checked protocol projections during the site build.
The site does not recreate method names, constants, or limits.
