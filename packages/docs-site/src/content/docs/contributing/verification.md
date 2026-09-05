---
title: Verification
description: Run Rottweiler's formatting, lint, test, code-generation, ownership, documentation, security, and package gates.
---

The main CI matrix runs on macOS and Linux. Start with the gates that match your
change, then run the complete set before delivery when practical.

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo run --locked --quiet -p xtask -- codegen --check
python3 scripts/check-dependency-direction.py
python3 scripts/check-ownership.py
python3 scripts/check-toolchain-ownership.py
python3 scripts/check-network-boundaries.py
```

Package checks:

```sh
bun run --cwd packages/plugin-sdk typecheck
bun test --cwd packages/plugin-sdk
bun run --cwd packages/plugin-sdk build
bun run --cwd packages/tui typecheck
bun test --cwd packages/tui
bun run --cwd packages/tui build
bun run --cwd packages/docs-site check
bun test --cwd packages/docs-site
bun run --cwd packages/docs-site build
```

Performance, security, SSH loopback, release, and protected soak gates are
tiered by workflow. Do not describe a queued or intentionally unrun gate as
passing.
