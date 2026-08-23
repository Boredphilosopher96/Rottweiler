---
title: Troubleshooting
description: Diagnose configuration, bundle, provider, trust, model, storage, and upgrade problems without guessing.
sidebar:
  order: 4
---

Start with local evidence:

```sh
rw --version
rw config check
rw doctor
rw trust status
```

## Provider or credential problems

Network checks are opt-in:

```sh
rw doctor --network
rw models list --refresh
```

Use `rw auth set-key <provider>` for API-key routes and `rw auth login
<provider>` for configured browser or device flows. Do not put secret values in
TOML to make a test pass.

## An extension does not load

Check project trust first. Invalid or unreadable artifacts are skipped with a
diagnostic; the rest of the application can still start. Re-run `rw trust
status` after any executable extension file changes.

## The application starts incompletely

Verify that you installed the complete release bundle. A standalone Rust binary
does not include the terminal executable, native renderer, WASM host, and plugin
host. Reinstall using [Installation](../installation.md).

## Upgrade or rollback

```sh
rw upgrade --channel stable
rw upgrade --rollback
```

Use `--channel beta` only when you intentionally accept a prerelease update
channel. Signed metadata prevents version rollback unless rollback is explicit.
