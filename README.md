# Rottweiler

Rottweiler is a provider-blind coding-agent harness with a headless Rust engine
and an OpenTUI frontend.

The complete product specification, architecture, decision log, roadmap, and
verification requirements begin in [PROJECT.md](PROJECT.md).

## M0 development commands

```sh
cargo run --locked -p rw-cli -- config check
cargo run --locked --quiet -p xtask -- codegen
cargo run --locked --quiet -p xtask -- codegen --check
cargo fmt --all -- --check
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
python3 scripts/check-dependency-direction.py
cargo deny check
cargo audit

cd packages/tui
bun install --frozen-lockfile
bun test
bun run typecheck
bun run build
```
