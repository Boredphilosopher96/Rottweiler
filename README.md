# Rottweiler

Rottweiler is a provider-blind coding-agent harness with a headless Rust engine
and an [OpenTUI](https://github.com/sst/opentui) frontend. It combines a fast,
responsive terminal experience with a client-independent engine, deterministic
provider replay, secure tool execution, and public extension protocols.

> **Project status:** the M0-M10 implementation is present, but this repository
> is still pre-v1. Release automation intentionally remains fail-closed until
> the required macOS/Linux measured performance baselines, eight-hour soaks,
> Terminal-Bench evidence, WSL2 acceptance, and 14 consecutive dogfood days
> have been collected on the protected runner classes.

## Why Rottweiler

- **Headless-first architecture.** The Rust engine owns sessions, tools,
  permissions, context, orchestration, storage, and providers. OpenTUI is one
  client over the authenticated protocol; future web and native clients do not
  require engine rewrites.
- **Provider-blind model IR.** Anthropic, OpenAI Chat/Responses-compatible
  APIs, ChatGPT subscription access, and GitHub Copilot subscription access are
  isolated adapters behind one internal streaming representation.
- **Replayable by design.** Provider calls, tool turns, failures, usage, and
  dynamic capability discovery have record/replay seams used throughout the
  test suite.
- **Secure defaults.** Sandboxed commands, capability-scoped plugins, explicit
  permission modes, folder trust, secret redaction, bounded storage, and
  guarded network boundaries are built in.
- **Batteries included.** Compaction, checkpoints and rewind, subagents,
  plan/discuss/execute modes, MCP client/server support, project intelligence,
  import/export, doctor, stats, and signed stable/beta updates ship in the core.
- **Extension parity.** Built-in tools, commands, hooks, agents, workflows, and
  modes use the same registries exposed to third-party TypeScript plugins.

## Repository map

| Path | Purpose |
|---|---|
| `crates/rw-core` | Headless session engine, context loop, permissions, orchestration |
| `crates/rw-cli` | CLI entrypoint, local supervisor, client protocol host |
| `crates/rw-providers` | Provider adapters, auth, routing, record/replay |
| `crates/rw-tools` | Built-in tools, background tasks, worktree isolation |
| `crates/rw-store` | Sessions, checkpoints, config, trust, credentials, memory |
| `crates/rw-sandbox` | macOS/Linux sandbox and egress enforcement |
| `crates/rw-mcp` / `crates/rw-ext` | MCP and extension protocol surfaces |
| `packages/tui` | Bun-compiled OpenTUI client |
| `packages/plugin-sdk` | TypeScript plugin SDK and conformance fixtures |
| `packages/plugin-docs` | Static plugin protocol documentation site |
| `protocol` | Versioned cross-language schemas and generated bindings |
| `evals` / `benchmarks` | Capability and performance gates |

## Build and run from source

Requirements are Rust 1.94.1 and Bun 1.3.14. On macOS or Linux:

```sh
git clone https://github.com/Boredphilosopher96/Rottweiler.git
cd Rottweiler

scripts/cargo-release.sh build --locked --release -p rw-cli
release_dir=$(scripts/cargo-release.sh artifact-dir)
bun install --cwd packages/tui --frozen-lockfile
bun run --cwd packages/tui build

ROTTWEILER_TUI_BIN="$PWD/packages/tui/dist/rottweiler-tui" \
  "$release_dir/rw"
```

Useful first commands:

```sh
"$release_dir/rw" --help
"$release_dir/rw" config check
"$release_dir/rw" doctor
"$release_dir/rw" models refresh
"$release_dir/rw" auth set-key <provider>
"$release_dir/rw" auth login <subscription-provider>
```

ChatGPT and GitHub Copilot subscription credentials are separate from ordinary
OpenAI API keys. Rottweiler performs its own reviewed login flows and does not
copy credentials from Codex, OpenCode, `gh`, VS Code, Copilot CLI, or Claude.
Claude subscription login is intentionally unsupported; Anthropic API keys are
supported. See [the feature specification](docs/01-FEATURES.md) and
[ADR-017](docs/03-DECISIONS.md#adr-017--consumer-subscription-providers-are-first-class-isolated-adapters)
for the exact boundary.

## Documentation

The implementation contract starts at [PROJECT.md](PROJECT.md):

- [Features](docs/01-FEATURES.md)
- [Architecture](docs/02-ARCHITECTURE.md)
- [Architecture decisions](docs/03-DECISIONS.md)
- [Extensibility and plugin protocol](docs/04-EXTENSIBILITY.md)
- [Security model](docs/05-SECURITY.md)
- [Milestone roadmap](docs/06-ROADMAP.md)
- [Verification, benchmarks, and release gates](docs/07-VERIFICATION.md)

## Development gates

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo run --locked --quiet -p xtask -- codegen --check
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
python3 scripts/check-dependency-direction.py
python3 scripts/check-network-boundaries.py
cargo deny check
cargo audit

bun install --cwd packages/tui --frozen-lockfile
bun run --cwd packages/tui test
bun run --cwd packages/tui typecheck
bun run --cwd packages/tui build
```

Release and nightly workflows add platform performance gates, fuzzing,
Terminal-Bench, real WSL2 acceptance, signed-update fixtures, exact-artifact
soaks, and the temporal dogfood ledger. Missing protected evidence blocks
publication rather than silently skipping a gate.

## License

Licensed under the [Apache License 2.0](LICENSE).
