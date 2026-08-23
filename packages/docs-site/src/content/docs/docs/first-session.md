---
title: First session
description: Validate Rottweiler, add one provider credential, inspect available models, and complete a first coding task.
sidebar:
  order: 2
---

Run these commands inside the repository you want Rottweiler to work on.

## 1. Check the installation

```sh
rw config check
rw doctor
```

`rw doctor` is network-free unless you add `--network`. It checks the product
bundle, storage, configuration, and local prerequisites without unexpectedly
contacting a provider.

## 2. Configure a route

User configuration normally lives at `~/.rottweiler/config.toml`. Define a
model alias and a provider without storing the secret in the file:

```toml
[models]
default = "fast"

[models.aliases]
fast = ["anthropic/<model-id>"]

[providers.anthropic]
kind = "anthropic"
api_key_credential = "providers.anthropic.api_key"
```

Replace `<model-id>` with an identifier returned by provider discovery. Store
the key through Rottweiler's hidden terminal prompt:

```sh
rw config check
rw auth set-key anthropic
rw models list --refresh
```

OAuth and device-flow providers use `rw auth login <provider>` instead. See
[Providers](./providers.md) for supported adapter kinds and gateway controls.

## 3. Run a task

Start the terminal application:

```sh
rw
```

Or run one bounded, non-interactive task:

```sh
rw -p "Map this repository and identify its three riskiest boundaries" \
  --permission-mode strict \
  --max-turns 12
```

Inside the TUI, `/help` lists live commands and keybindings. `/models`,
`/permissions`, `/context`, `/agents`, `/mcp`, `/rewind`, and `/review` are good
starting points.

## 4. Continue the work

```sh
rw sessions list
rw --continue
```

Use `rw --resume <session-id>` when you need an exact session rather than the
most recently updated one.
