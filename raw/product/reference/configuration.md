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
