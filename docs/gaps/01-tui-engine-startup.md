# 01 — TUI ↔ engine startup

> **STATUS (2026-07-12, post-review):** commits `42935b9` (fail fast when engine startup exits), `400b8dd` (reject competing workspace engines promptly), and `ee12918` (preserve watchdog barrier on direct resume) landed after this file was written and target these findings. **Re-verify each item below against HEAD before working it.** The maintainer has since reached a live TUI session, so the hard startup blocker appears at least partially fixed; the in-session failures that remain are in [09](09-tui-interaction-and-in-app-settings.md).

The OpenTUI frontend is the reason TypeScript/Bun are in the stack at all (ADR-001). At review time it did not reach a usable state.

## GAP-01-01 — Interactive TUI hung forever on "connecting · attempt 0" — **P0 [verified at review time]**

**Repro (at review time).** From a clean workspace, `rw` (no args) → grant trust → the OpenTUI shell renders (title bar, input box, status line `◉ execute │ model fast │ ctx — │ $— │ cache — │ git —`) and then sits on `waking the engine… / connecting · attempt 0` indefinitely. With `--dangerously-trust` and a fresh `ROTTWEILER_SESSION_ID`, same result. The non-TUI supervisor path surfaced `engine did not become ready: engine exited before authenticated readiness (exit status: 1); another Rottweiler process may already own this session`.

**What worked, isolating the fault.** `rw -p "…"` (in-process engine) ran full turns. `rw serve --socket <path in a real dir> --session <fresh id> --workspace <dir>` came up and bound its socket. So the engine binary was fine; the *supervised* launch failed. Causes triggered while narrowing:
- `rw serve` with the default session from a workspace with prior sessions → `host query failure: session workspace is outside authorized roots` (`crates/rw-cli/src/host_runtime.rs:1199`).
- `rw serve` with a socket whose parent dir is a symlink (`/tmp` → `/private/tmp` on macOS) → `server runtime root is not a real directory` (`crates/rw-cli/src/server.rs:272`).
- The default TUI session is a fresh id (`runtime::select_interactive_session` → `new_session_id()`), so the remainder was a handshake/readiness race — the attempt counter never left 0.

## GAP-01-02 — Engine child stderr is discarded, making startup failures undiagnosable — **P0 [code]**

`crates/rw-cli/src/supervisor.rs:690` set the engine child's stdio to `StdioMode::Null` for all non-replay launches. When the engine exits before readiness, the operator sees only "engine exited before authenticated readiness" — the actual error is thrown away. Check whether `42935b9` fixed the *surfacing* (including the child's stderr tail in the error) or only the fail-fast timing; the requirement is that the next startup failure is diagnosable from the error message alone.

## GAP-01-03 — `session workspace is outside authorized roots` on default serve — **P1 [verified at review time]**

`rw serve` from a workspace with any prior session (default session resolution) failed authorization at `host_runtime.rs:1189-1203`: a session created in workspace A cannot be served from workspace B, and default resolution can select a cross-workspace session. The check is correct defense-in-depth; the default session *selection* feeding it a foreign workspace was the bug. Possibly addressed by `400b8dd` — re-verify.

## GAP-01-04 — `rw replay` (default TUI render path) is broken — **P1 [verified]**

`rw replay <session-id>` → `× No such file or directory (os error 2)` from every working directory, for a session id that `rw export <same id>` renders fine. Only `rw replay <id> --jsonl` works (bypasses `run_history_replay_with_tui`, `main.rs:1250`). The event log loads correctly; the TUI-render replay path opens a missing file (likely the compiled TUI/dylib via a bad relative path) and fails with a bare ENOENT.

**Fix.** Resolve the bundle the way the live launcher does (`locate_tui_executable`), and name the missing path in the error.
