# Rottweiler TUI

The OpenTUI/Bun client for the headless Rust engine. It imports
`../../protocol/types.ts`, which is generated from `rw-types`; protocol shapes
must not be handwritten in this package.

```sh
bun install
bun run dev
bun test
bun run typecheck
```

The M0 renderer test uses OpenTUI's public `@opentui/core/testing` surface and
captures the native renderer's in-memory character and styled-cell buffers.
This is the foundation for M4 golden-screen and latency tests.
