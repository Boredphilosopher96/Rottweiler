---
title: Configuration reference
description: Configuration discovery, precedence, project trust, typed provider records, and security-sensitive user-only settings.
sidebar:
  order: 2
---

Configuration is TOML. `rw config check` is the authoritative way to view the
effective value and provenance for an installed version.

## User file discovery

Rottweiler uses the first applicable user configuration root:

1. `$ROTTWEILER_HOME/config.toml`
2. `$XDG_CONFIG_HOME/rottweiler/config.toml`
3. `$HOME/.rottweiler/config.toml`

The project file is `<workspace>/.rottweiler/config.toml`.

## Precedence

Values resolve in this order, with later layers winning:

1. built-in defaults;
2. user configuration;
3. trusted project configuration;
4. environment overrides;
5. CLI overrides.

Project configuration is not a path to user secrets or global security policy.
Provider definitions, permissions, network and proxy policy, sandbox rules,
telemetry, and update channel remain user-scoped.

## Core sections

The configuration model covers:

- `engine` for session and subagent concurrency;
- `models` and `models.aliases` for provider-neutral routing;
- `compaction` and `budget` for context and spend guardrails;
- `providers` for typed provider adapters and credential references;
- `network` and `websearch` for guarded outbound access;
- `permissions` and `sandbox` for execution policy;
- `toolchain` for declarative formatter, linter, and test hooks;
- `telemetry`, `updates`, and `ui` for their explicit product settings.

MCP servers are stored separately in user-scoped `mcp.toml`. Commands, skills,
agents, modes, workflows, and plugins are discovered from their own extension
files and manifests; `config.toml` does not redefine those owners.

Exact accepted fields are versioned with the binary. Use:

```sh
rw config check
rw doctor
```

Unknown or unsafe configuration fails at the boundary instead of becoming an
untyped bag passed into runtime logic.

## Example

```toml
[models]
default = "coding"

[models.aliases]
coding = ["anthropic/<model-id>"]

[providers.anthropic]
kind = "anthropic"
api_key_credential = "providers.anthropic.api_key"
```

Store the referenced value with `rw auth set-key anthropic`.

## Budget guardrails

Choose limits that match the route's accounting unit. API-key routes report
micro-US-dollars, consumer subscription routes report tokens, and Copilot
routes can report AI credits.

```toml
[budget]
session_cost_cap_micros_usd = 5000000
daily_cost_cap_micros_usd = 20000000
session_token_cap = 250000
daily_token_cap = 1000000
token_rate_alarm_per_minute = 100000
warn_at_percent = 80
```

Rottweiler stops before another provider call after a hard cap. If a configured
cap cannot be measured because a provider omits its accounting unit, the cap
fails closed.

## Automatic verification

Configure one test command to run after every otherwise-successful agent turn:

```toml
[toolchain]
formatter = "cargo fmt -- {file}"
linters = ["cargo clippy --offline --workspace --all-targets -- -D warnings"]
test = "cargo test --workspace"
```

Formatter and linter commands run after matching file edits. The test command
runs once at the turn boundary. A failing test marks the turn failed and adds a
bounded diagnostic to durable conversation context so the next turn can act on
it.

For toolchains installed outside the system runtime paths, declare their read
roots explicitly in trusted configuration:

```toml
[toolchain]
runtime_read_roots = ["/home/alice/.cargo/bin", "/home/alice/.rustup"]
formatter = "rustfmt {file}"
```

Supply existing absolute paths for your installation, up to 32 paths of 4096
UTF-8 bytes each. These read-only grants belong to formatter, linter, and test
commands. Ordinary Bash and declarative shell hooks do not inherit them; writes
remain limited to the workspace and private scratch, network access remains
denied, and sensitive credential paths remain excluded. Linux adds these roots
to its system-read baseline; macOS retains its general-read policy with
credential exclusions. Changing PATH or HOME does not grant read authority.
Project toolchain configuration takes effect only after project trust.
