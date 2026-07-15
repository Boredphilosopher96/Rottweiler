# Rottweiler TUI

The OpenTUI/Bun client for the headless Rust engine. It imports
`../../protocol/types.ts`, which is generated from `rw-types`; protocol shapes
must not be handwritten in this package.

```sh
bun install
bun run dev
bun run test       # deterministic functional suite
bun run test:perf  # isolated latency and frame-compute gates
bun run typecheck
```

The M0 renderer test uses OpenTUI's public `@opentui/core/testing` surface and
captures the native renderer's in-memory character and styled-cell buffers.
This is the foundation for M4 golden-screen and latency tests.

## Keybindings

The standard preset preserves the shipped shortcuts (`ctrl+p` commands,
`ctrl+m` models, `ctrl+o` modes, `ctrl+s` sessions, `ctrl+r` review,
`ctrl+g` child-agent tree, `shift+tab` agent mode, `ctrl+e` external editor).
The child tree supports keyboard or mouse drill-in, live transcript inspection,
Escape back to the parent, and follow-up/interrupt/close actions from `ctrl+p`.
Set the TUI-only
`keybindings.toml` section to use Vim mode and override bindings by action:

```toml
preset = "vim"

[bindings.vim_normal]
open_command_picker = ["space"]
focus_next = [] # explicitly unbind Tab
```

Vim mode starts in `NORMAL` with the composer target. `i`/`a` enter insertion;
Escape or `ctrl+[` returns to normal mode; Tab moves normal-mode focus between
the composer and transcript. Transcript and picker navigation use `j`/`k`,
`ctrl+u`/`ctrl+d`, and `g`/`G`. A picker opens in query insertion: the first
Escape enters picker normal mode and the second closes it. The status line
always shows the active Vim mode and normal-mode target.

The launcher resolves the first available file in this order: trusted project
`.agents/keybindings.toml`, trusted project `.rottweiler/keybindings.toml`, user
`~/.agents/keybindings.toml`, then user `~/.rottweiler/keybindings.toml`.
Project bytes must match the exact folder-trust inventory; remote and historical
replay sessions use user configuration only. The launcher forwards the bounded
TUI-only TOML as `ROTTWEILER_TUI_KEYBINDINGS`. Parsing is bounded to 64 KiB.
Unknown contexts/actions, malformed strokes,
duplicate bindings, and bindings shadowed by a global shortcut fail with an
actionable configuration error instead of being resolved by order.

`ctrl+c` is deliberately not configurable: the OpenTUI renderer owns it for
immediate process exit. Focused approval, question, plan, and review panels
likewise retain their safety keys (`Enter`, selection navigation, and review
`A`/`R`) so a global binding cannot make an authorization decision unreachable.
