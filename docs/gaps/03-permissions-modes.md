# 03 — Permissions & modes

## GAP-03-01 — `auto-safe` denies *all* file writes → no automation mode that can actually code — **P1 [verified + code]**

**Resolved (2026-07-12).** `auto-safe` admits only `write`, `edit`, and `multi_edit` calls whose single real path stays within a canonical active root, including `@root/N`; exec, network, missing/ambiguous paths, outside paths, and symlink escapes still deny, and explicit deny rules remain absolute (`340188f`).

**Repro.** `rw -p "create hello.py that prints hi, then run it" --permission-mode auto-safe` → the agent replies *"I'm blocked by workspace permissions: the available file-write and shell tools are currently denied"* and writes nothing. The identical prompt under `--permission-mode yolo` creates and runs the file end-to-end.

**Cause.** `crates/rw-core/src/permission.rs:577-612`. Headless `AutoSafe` default is `Deny`; for unsandboxed tools it returns `Deny` unconditionally (`:589`); for sandboxed tools it allows only `safe_listed || is_read_only` (`:607`). `write`/`edit`/`multi_edit` are mutating file tools, so auto-safe denies them across the board.

**Why it's a gap, not a policy choice.** The three headless modes are `strict` (ask everything — unusable non-interactively), `auto-safe` (deny every mutation — can't code), and `yolo` (allow everything — unsafe). There is no mode for what a CI coding agent actually needs: **allow reversible in-workspace file writes, deny network/exec/out-of-workspace.** File writes are the most checkpoint-recoverable operation in the system, yet they're lumped with the dangerous ones. Every non-interactive coding use is pushed to `yolo`, defeating the graduated permission design.

**Fix.** Redefine `auto-safe` to auto-allow writes/edits targeting the workspace roots (checkpointed, revertible) plus read-only and safe-listed commands, while denying/asking for network, exec outside the safe-list, and writes outside the workspace.

## GAP-03-02 — Permission-decision path is otherwise sound — **[code, positive]**

Canonicalization and rule machinery per 05-SECURITY are implemented (`permission.rs`): argv parsing, safe-list interaction, session/project remembered approvals with hash-tracked binding, MCP-tool default to network+exec. Interactive policy folds safe-list into `Ask→Allow` only when sandboxed. No defect beyond GAP-03-01.

## GAP-03-03 — Trust gate holds; security-sensitive project keys are refused — **[code, positive]**

Layer 0 folder trust is implemented with the round-2 hardening: `rw trust {status,grant,revoke}`, inert project config until granted, security-sensitive keys user-level-only. One UX defect filed separately: the trust prompt fires even when the executable inventory is `(none)` — pointless friction on every fresh workspace (see also 09's "everything must be quieter/smoother in the app" theme).
