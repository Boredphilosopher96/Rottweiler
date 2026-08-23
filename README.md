<p align="center">
  <img src="docs/assets/rottweiler-logo.png" width="180" alt="Rottweiler mascot logo">
</p>

# Rottweiler

Rottweiler is a fast, local coding-agent harness with a headless Rust engine and
a compiled OpenTUI client. Interactive work, one-shot automation, remote use,
MCP, replay, permissions, durable sessions, and extensions all meet the same
engine boundaries.

![Rottweiler coordinating coding work](docs/assets/rottweiler-hero.png)

## Why Rottweiler

- **One engine for every client.** The TUI, print mode, remote clients, and MCP
  do not grow separate routing, permission, persistence, or tool behavior.
- **Durable work.** Resume, search, replay, rewind, fork, checkpoint, and export
  sessions without making the terminal process the owner of history.
- **Explicit authority.** Project trust, permission modes, canonical workspace
  roots, sandboxing, guarded network access, and credential isolation are
  engine boundaries.
- **Provider-neutral sessions.** Route aliases across Anthropic, OpenAI,
  ChatGPT subscription, GitHub Copilot, and typed OpenAI-compatible gateways.
- **Open extension surfaces.** Commands, skills, agents, modes, workflows,
  hooks, MCP, WASM components, and RPC plugins use documented contracts.
- **Inspectable use.** Context, tokens, cache behavior, cost or provider
  credits, compaction, and tool activity remain visible.

## Install the latest signed release

Apple silicon macOS:

```sh
brew install --cask Boredphilosopher96/tap/rottweiler
rw doctor
rw
```

x86-64 Linux and WSL2 use the version-pinned bootstrap shown on the
[installation page](https://boredphilosopher96.github.io/Rottweiler/docs/installation/).
That command is projected from signed release metadata and verifies the exact
archive length and SHA-256 digest before installation.

The [platform reference](https://boredphilosopher96.github.io/Rottweiler/docs/reference/platforms-and-releases/)
projects the current signed targets from release metadata. The macOS cask is
checksum-verified but not Apple-notarized.

## First task

Configure one provider, then run the TUI with `rw` or one bounded prompt:

```sh
rw config check
rw models list --refresh
rw -p "Map this repository and identify its riskiest boundary" \
  --permission-mode strict \
  --max-turns 12
```

Use `rw auth set-key <provider>` for an API key and `rw auth login <provider>`
for a configured browser or device flow. Credential values do not belong in
TOML.

## Documentation

The [Rottweiler documentation site](https://boredphilosopher96.github.io/Rottweiler/)
contains installation, tutorials, guides, CLI/configuration reference,
architecture, security, API schemas, and contributor verification.

For coding agents:

- [`llms.txt`](https://boredphilosopher96.github.io/Rottweiler/llms.txt) —
  compact map;
- [`llms-full.txt`](https://boredphilosopher96.github.io/Rottweiler/llms-full.txt)
  — complete public corpus;
- [`docs-index.json`](https://boredphilosopher96.github.io/Rottweiler/docs-index.json)
  — versioned page catalog with raw Markdown URLs and source owners.

The site describes one product surface and projects release data, API schemas,
and other machine-owned facts directly from their repository owners.

## Build from source

Source builds require Rust 1.97.1 and Bun 1.3.14. The release builder is the
complete build recipe; it compiles the CLI, TUI, native renderer, WASM host,
TypeScript plugin host, and SDK, then validates the archive contract.

```sh
git clone https://github.com/Boredphilosopher96/Rottweiler.git
cd Rottweiler
rustup toolchain install 1.97.1 --profile minimal
bun install --cwd packages/tui --frozen-lockfile
scripts/build-release.sh
```

## Repository map

| Path | Owner |
|---|---|
| `crates/rw-core` | Provider-neutral session engine and orchestration |
| `crates/rw-runtime` | Production storage, provider, tool, MCP, and extension composition |
| `crates/rw-cli` | Public `rw` entrypoint and supervised product lifecycle |
| `crates/rw-types` | Shared config and engine-client protocol types |
| `crates/rw-plugin-protocol` | Plugin API values, validation, limits, and projections |
| `packages/tui` | OpenTUI interaction and rendering |
| `packages/plugin-sdk` | TypeScript plugin SDK and project scaffold |
| `packages/plugin-host` | Private compiled TypeScript source-plugin host |
| `packages/docs-site` | Public documentation, human and agent projections, and Pages overlay |
| `contracts/release-contract.json` | Release archive shape and bundle membership |
| `architecture/ownership.toml` | Enforced semantic ownership boundaries |

Read [PROJECT.md](PROJECT.md) before changing code. Report vulnerabilities
through the private process in [SECURITY.md](SECURITY.md).

Licensed under Apache-2.0. See [LICENSE](LICENSE).
