# 06 — Built-in tools

Mostly healthy — the tools do what the spec says. Findings are minor.

## Positive confirmations — **[code/verified]**

- **`edit` matching** (`rw-tools/src/files.rs:714-761`): exact match first, whitespace-normalized fallback, and ambiguous matches fail with candidate locations listed — the round-1 contract, implemented.
- **`bash` streaming** (`rw-tools/src/bash.rs`): streaming sink with cross-chunk redaction pattern spanning. Real, not buffered-only.
- **`webfetch`**: SSRF guard present (05/GAP-05-03); egress-domain normalization wired.
- **Tool set complete** vs `/context`: read, write, edit, multi_edit, grep, glob, ls, bash, webfetch, todo, ask_user, spawn_agent, symbols, definition/references/rename/diagnostics, background_{status,output,kill}, apply_worktree_diff. `websearch` is **absent** (deferred to M6 in the roadmap — noted so it isn't forgotten).
- **`symbols`** (tree-sitter) registered and in context.

## GAP-06-01 — Checkpoint-for-bash "unrestorable" surfacing needs runtime confirmation — **P2 [code]**

The protocol has `UnrestorablePath`/`unrestorable_paths` (observed in events), so the plumbing exists, but a bash-created-file rewind wasn't reproduced to confirm the file is reported unrestorable rather than silently lost. Fixture: `bash "echo x > new.txt"` → `/rewind` → assert `new.txt` in the unrestorable set.

## GAP-06-02 — `webfetch` per-domain approval flow unverified — **P2 [code]**

The SSRF hard-deny is confirmed; the interactive ask-per-new-domain flow (with "always" persisting into the egress allowlist) was not exercised — it needs the interactive TUI (see 09). Verify once the TUI approval flow works.
