Rottweiler is one application made from several supervised processes. Install
the complete bundle; installing only the Rust CLI does not install the terminal
frontend, native renderer, WASM host, or plugin host.

## macOS with Homebrew

The Homebrew cask supports Apple silicon macOS.

```sh
brew install --cask Boredphilosopher96/tap/rottweiler
rw --version
rw doctor
```

Upgrade later with:

```sh
brew upgrade --cask rottweiler
```

Homebrew verifies the release checksum. The cask is not Apple-notarized.

## Linux or WSL2

The installer supports x86-64 Linux and WSL2. Download the version-pinned
bootstrap, inspect it if your environment requires that, then run it. The
bootstrap verifies the archive length and SHA-256 digest before invoking the
installer inside the archive.

```sh
curl --fail --location --proto '=https' --tlsv1.2 \
  --output rottweiler-install.sh \
  https://github.com/Boredphilosopher96/Rottweiler/releases/download/v0.1.4/rottweiler-install.sh
sh rottweiler-install.sh
rw --version
rw doctor
```

Do not rewrite the URL to `latest`: the bootstrap is intentionally pinned to
one version and one set of archive digests.

## Build from source

Source builds require Rust 1.97.1, Bun 1.3.14, and the normal native build tools
for your platform. The release builder is the sole source-build recipe because
it compiles and validates every required sibling executable.

```sh
git clone https://github.com/Boredphilosopher96/Rottweiler.git
cd Rottweiler
rustup toolchain install 1.97.1 --profile minimal
bun install --cwd packages/tui --frozen-lockfile
scripts/build-release.sh
```

The command prints the validated archive path. Extract that archive and run its
`install.sh`, or use it as the complete portable bundle.

## Next step

Continue with [First session](./first-session.md) to configure a provider without
putting a credential in TOML.
