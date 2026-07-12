# 02 — Context engine & compaction

Compaction was specified as a 1:1 opencode port (ADR-010) and is a headline reason the project exists. The *algorithm* is ported faithfully; the *wiring* breaks for subscription auth — which is the maintainer's own config (`~/.rottweiler/config.toml`: `kind = "openai_codex"`).

## GAP-02-01 — Subscription models report no context window → meters read 0 and auto-compaction never fires — **P0 [verified]**

**Resolved (2026-07-12).** Subscription and Copilot adapters receive context/output capabilities from validated user model metadata, and live discovery is the availability authority. Unknown capacity is explicitly represented as unknown rather than falsely exhausted.

`crates/rw-core/src/provider_factory.rs:1894` `subscription_model_capabilities()` hardcodes `max_context_tokens: None` and `max_output_tokens: None` (same for `github_copilot_capabilities()` at `:1906`). Consequences, all observed:

- **Budget meters are dead.** The print-mode `context_usage_updated` event carries `"usable_tokens":"0","reserved_tokens":"0"` and the TUI status line shows `ctx —` / `$—` / `cache —`. `engine.rs:9408` computes `(usable, reserved)` via `metadata.max_context_tokens.map_or((0,0), …)`, so a `None` window zeroes everything.
- **Automatic compaction never triggers.** The ported overflow check (`rw-context/src/budget.rs`, mirroring opencode `overflow.ts`) treats a zero/None window as "no limit" (opencode returns `false` when `context === 0`). So for subscription users the auto-compaction path — the M3 deliverable — is unreachable. Manual `/compact` still works.
- **Vision is hardcoded `false`** on the subscription path, so image input can never work there.

The data is *available* and ignored: the pricing table has `max_context_tokens = 400000` for `gpt-5.4-mini`. The subscription path just doesn't consult it. (Note: fixing this must respect the maintainer's directive that the *selection catalog* is dynamic — see 08 — but capability *enrichment* from the refreshable table for a known model id is fine and is exactly what the API-key path already does at `provider_factory.rs:1968`.)

**Fix.** Resolve subscription/Copilot model capabilities from the catalog (live discovery enriched by the refreshable table), falling back to a conservative default only when the model is unknown. One change re-lights the meters and the compaction trigger together.

## GAP-02-02 — `usable_tokens: 0` poisons every downstream consumer — **P1 [code]**

**Resolved (2026-07-12).** Context snapshots/events now carry `context_window_known` plus a bounded reason, so zero is never ambiguously interpreted as a real zero-token window.

Anything that thresholds against usable/reserved is degenerate while they're 0: spend-rate alarms, `/context` budget line, cache-hit display, doom-loop budget guard. Beyond fixing GAP-02-01, add a guard: a `None`/0 window is treated as *unknown* (meters show `—` with a one-line reason, triggers disabled explicitly) rather than silently 0 — so the next provider with incomplete caps can't reintroduce this class of bug.

## GAP-02-03 — Compaction constants are correct — **[code, positive]**

`rw-context/src/prune.rs` implements ADR-010 faithfully: `DEFAULT_PROTECTED_TOOL_TOKENS = 40_000`, `DEFAULT_MINIMUM_RECLAIM_TOKENS = 20_000`, `DEFAULT_RECENT_USER_TURNS = 2`, protected tools `{"skill"}`, summary/prune stop markers, pins additive. `reserved = min(20_000, max_output)` in `budget.rs:154`. `ToolOutputPruned` events exist and persist. Once GAP-02-01 lands, add the long-session auto-compaction golden test from the M3 AC — it cannot currently be exercised on a subscription account.

## GAP-02-04 — All tool definitions render `pinned: true` in `/context` — **P2 [verified]**

**Resolved (2026-07-12).** Tool-schema context entries no longer claim user pinning; the context inspector retains the distinction between intrinsic tool definitions and explicitly pinned items (`340188f`).

Every `tool:*` item in the `/context` snapshot shows `"pinned": true`. If that's a real pin, tool schemas can never be pruned/evicted — meaning context surgery can't touch the largest fixed cost. Confirm intended semantics vs defaulting bug.
