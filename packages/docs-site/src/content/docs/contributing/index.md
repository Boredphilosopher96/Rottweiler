---
title: Contributing
description: Build the complete product, follow repository ownership boundaries, and use the checked architecture and verification documents.
---

Read `PROJECT.md` before changing code. It records the product tenets and maps
the maintainer architecture documents.

## Build the complete product

```sh
rustup toolchain install 1.97.1 --profile minimal --component clippy,rustfmt
bun install --cwd packages/tui --frozen-lockfile
python3 scripts/build-native-candidate.py --print archive
```

The release builder compiles and validates every required sibling executable.

## Preserve ownership

- Add a fact to its domain owner, then generate or check projections.
- Do not copy protocol values into docs, SDKs, hosts, or tests.
- Keep implementation, callers, schemas, and fixtures aligned with the owned
  contract. Reject inputs outside that contract.
- Keep UI behavior in the client and session behavior in the Rust engine.
- Keep `/updates/**` under the release workflow's exclusive ownership.

## Before opening a pull request

Run the focused checks for the code you changed, then follow
[Verification](./verification.md) for repository gates.
