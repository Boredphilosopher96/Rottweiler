# 04 — CLI & UX surface

Each of these is a paper-cut a real user hits in the first session.

## GAP-04-01 — Session titling never runs; every session is "New session" — **P1 [verified]**

`rw sessions search <anything>` returns rows all titled `New session` (13 sessions at review time). The `title` model alias is configured and the store has a `title` column + FTS index (`rw-store/src/session.rs:423,1428`), but nothing generates a title from the first turn. Session discovery is therefore badly degraded. **Fix:** after the first assistant turn, generate a title via the `title` alias and update the session record.

## GAP-04-02 — No way to inspect models: `rw models` has only `refresh` — **P1 [verified]**

No `models list` / `models show <id>`, so a user cannot see what context window, pricing, or capabilities the harness believes a model has — exactly what's needed to diagnose GAP-02-01. Per the maintainer's correction in 08: these commands must read the **live dynamic catalog** (provider discovery), not `models.toml`.

## GAP-04-03 — `rw sessions` has only `search`; no `list` / `recent` — **P1 [verified]**

`sessions list` and `sessions recent` both error. With all titles identical (GAP-04-01), `search` is nearly useless for "show my recent sessions," and there's no browsing surface for `--resume`.

## GAP-04-04 — `rw doctor` takes ~23 seconds with no network probe — **P1 [verified]**

`time rw doctor` → **22.8 s** wall at ~0% CPU with reachability `[SKIP]`ped — it's sleeping on something (keychain or sandbox probe timeout). `rw config check` is ~10 ms. Doctor is what people run when something is already wrong; it must be fast, or say what it's waiting on.

## GAP-04-05 — `rw replay <id>` argument contract is confusing — **P2 [verified]**

`replay` rejects both a bare session id (ENOENT — see 01/GAP-01-04) and a path (`session id is empty, too long, or contains unsafe characters`), while `export` accepts the same id. Align the contract with `export` and name the missing path in errors.

## GAP-04-06 — Print-mode JSON leaks raw provider reasoning blobs — **P2 [verified]**

`rw -p … --output-format json` emits `thinking` blocks whose `signature` contains the full base64 `encrypted_content` reasoning payload (hundreds of lines per turn). Strip or truncate encrypted reasoning signatures in JSON output.

## GAP-04-07 — Export redaction mangles its own content — **P2 [verified]**

`rw export <id>` (markdown) renders timestamps as `\[REDACTED\]` and rewrites command help text — `/add-dir <path>` becomes `[REDACTED_PATH]` — because the export redactor treats slash-prefixed *command names* as filesystem paths and redacts times wholesale in the user's own export. Redaction at the export boundary should target secrets and machine-local paths, not the transcript's own UI strings.
