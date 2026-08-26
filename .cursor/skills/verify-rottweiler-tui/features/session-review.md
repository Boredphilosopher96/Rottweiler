# Session Review

Session Review is the full-primary cumulative diff reached through `/review`. It keeps the existing fingerprint-bound per-file accept and revert commands while replacing the rounded bottom modal with the design's aligned content and detail regions.

## User paths

- `/review` requests the current cumulative review and focuses the retained file list.
- Up and Down select a file without changing its decision.
- `a` accepts the selected file using its displayed fingerprint.
- `r` reverts the selected file only when the engine retained an original checkpoint.
- Escape closes review and restores the prior input focus.
- The composer and one-row status remain visible for session context while the review list owns focus.

## Production proof

Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh session-review /tmp/rottweiler-tui-evidence/<run-id>`.

Require `session-review.{txt,ansi,png,json}` at 110 by 32 and `session-review-narrow.{txt,ansi,png,json}` at 72 by 18. Both captures must come from `/review` through the production renderer, every JSON assertion must pass, and no SVG may exist.

The wide proof pins the 73-cell review region, divider at column 73, 37-cell detail region, derived file and changed-line totals, selected-file status, and decision counts. The narrow proof removes the detail rail and preserves the complete selected diff and decision keys.

## Invalid evidence

- A directly mounted component that bypasses `/review`.
- Worktree branch, ahead, stash, actor, hunk, or bulk-decision claims not present in the review projection.
- Revert offered for a file whose original bytes were not checkpointed.
- A rounded modal over visible conversation cells.
- SVG text or individually positioned glyph rendering.

## Focused checks

- `cd packages/tui && bun test test/components.test.ts -t "review"`
- `cd packages/tui && bun test test/app.test.ts -t "review|changed-file diff|workspace diff"`
- `cd packages/tui && bun test test/visual-harness.test.ts -t "session review"`
