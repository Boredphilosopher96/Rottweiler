# Working in Rottweiler

Read `PROJECT.md` and the maintainer documents relevant to the change before editing.
Keep communication concise and direct. A question alone is not authorization to change code.

## Code structure

- Handwritten source and test files must contain at most 1,500 lines, including comments and blanks. Run `python3 scripts/check-source-size.py` before delivery.
- Split by responsibility and behavior. Use small module entrypoints with explicit dependencies. Do not replace a large file with numbered fragments, blanket lint exceptions, or compatibility wrappers.
- Generated outputs are recognized through `architecture/ownership.toml` and their generator markers. Do not label handwritten code as generated to bypass the limit.
- Migrate all callers and delete superseded APIs. Backward compatibility is not a requirement.

## Build artifacts and cleanup

- Give each concurrent worktree its own Cargo target directory. Never share a target directory between different source trees.
- Reuse that worktree's target directory across checks. Avoid creating a fresh multi-gigabyte build cache for each command.
- Keep source stable while an integrated build/test run is executing. Integrate changes before starting the run; do other work in an isolated worktree.
- Before declaring work done, retain useful failure logs, benchmark samples, and source/artifact identities in the designated evidence location.
- Then run `python3 scripts/clean-build-artifacts.py` to inspect this workspace's build output. Repeat with `--apply` after its builds and tests have stopped. `--worktrees` includes every registered worktree: use it for preview, and only apply when all affected work is finished.
- Remove temporary worktrees created for the task after their changes are integrated and their evidence retained. Never force-remove a dirty worktree or delete another task's worktree.
- For task-created Cargo targets outside a worktree, pass each exact directory with `--target-dir PATH`. This selects only those targets, so it can clean an inactive task without touching ongoing builds. Clean them after verification too.
- Dependency installations are retained by default. Use `--dependencies` only when intentionally removing `node_modules` as well. Do not delete user configuration, credentials, sessions, recordings, Git data, or retained evidence.
