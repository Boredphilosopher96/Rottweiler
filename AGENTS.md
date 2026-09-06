# Working in Rottweiler

Read `PROJECT.md` and the maintainer documents relevant to the change before editing.
Keep communication concise and direct. A question alone is not authorization to change code.

## Code structure

- Handwritten source and test files must contain at most 1,500 lines, including comments and blanks. Run `python3 scripts/check-source-size.py` before delivery.
- Split by responsibility and behavior. Use small module entrypoints with explicit dependencies. Do not replace a large file with numbered fragments or blanket lint exceptions.
- Generated outputs are recognized through `architecture/ownership.toml` and their generator markers. Do not label handwritten code as generated to bypass the limit.
- After editing GitHub workflows, run `scripts/check-workflows.sh` before pushing. It validates expression contexts and job syntax that generic YAML parsing cannot check.
- Implement one coherent product design. Keep its implementation, callers, schemas, fixtures, tests, and documentation aligned. Define required inputs explicitly and reject data outside those contracts. Describe behavior and rationale directly; keep development chronology, progress reports, and iteration narratives out of the product repository. Provider protocols and terminal input conventions are external interfaces with their own semantics.

## Build artifacts and cleanup

- Use `python3 scripts/build-native-candidate.py` for native products. Pass its verified candidate directory to acceptance gates; do not hide compilation inside measurement scripts. The builder reuses an unchanged source/toolchain/target/profile and owns strict product size checks.
- Before source TUI tests or Rust integration tests that render the TUI, export `ROTTWEILER_OPENTUI_LIBRARY="$(python3 scripts/build-opentui-native.py)"`. The source preload verifies this artifact; measured commands never compile or select an unverified npm renderer.
- Before Rust tests that start native plugins, run `ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT="$(python3 scripts/build-test-helper.py)" && export ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT` using the same worktree target. Test harness binaries cannot dispatch sandbox worker entrypoints; the explicit helper keeps their process policy identical to native execution.
- Give each concurrent worktree its own Cargo target directory. Never share a target directory between different source trees.
- Reuse that worktree's target directory, build profile, and feature selection across checks. Avoid creating a fresh multi-gigabyte cache or dependency variant for each command.
- Inspect free disk space and the cleanup inventory before large builds. After a disk-space failure, stop builds and clean inactive task output before retrying.
- Keep source stable while an integrated build/test run is executing. Integrate changes before starting the run; do other work in an isolated worktree.
- Keep operational logs and verification results outside the product source tree. Run the checks needed to establish behavior before delivering changes.
- Then run `python3 scripts/clean-build-artifacts.py` to inspect this workspace's build output. Repeat with `--apply` after its builds and tests have stopped. `--worktrees` includes every registered worktree: use it for preview, and only apply when all affected work is finished.
- Remove temporary worktrees created for the task after their changes are integrated and their evidence retained. Never force-remove a dirty worktree or delete another task's worktree.
- For task-created Cargo targets outside a worktree, pass each exact directory with `--target-dir PATH`. This selects only those targets, so it can clean an inactive task without touching ongoing builds. Clean them after verification too.
- Dependency installations are retained by default. Use `--dependencies` only when intentionally removing `node_modules` as well. Do not delete user configuration, credentials, sessions, recordings, Git data, or retained evidence.
