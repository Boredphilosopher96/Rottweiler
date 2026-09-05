# Build hygiene and source ownership

The cleanup command previews disk use by default. `--apply` removes only known
Cargo targets and compiled bundle directories. `--target-dir PATH` selects only
those exact Cargo caches, so an inactive worktree can be cleaned while another
is still building. Parent aliases such as macOS `/tmp` are normalized; a
symlink at the selected directory is rejected. Workspace roots, ancestors,
tracked content, and Cargo directories without artifact markers are rejected.
Dependencies require `--dependencies`; sessions, configuration, credentials,
Git data, and evidence directories are outside the default candidate set.

On September 4, the command removed the inactive `rw-context-sizing` and
`rw-tool-scheduling` targets from this task. Preview estimated 2.85 GiB; Cargo
reported 5,928 and 4,987 removed files. A second preview returned zero bytes.
Active worktree caches were retained for ongoing verification. Every registered
worktree is protected from deletion, even when selecting an external target.

`AGENTS.md` requires separate reusable Cargo targets per worktree, stable source
during integrated verification, evidence retention, and cleanup after work
finishes. README documents the user-facing commands.

The source-size gate counts physical lines, including blank lines and comments,
and rejects handwritten source and tests above 1,500 lines. It includes tracked
and untracked non-ignored files. Generated outputs must be registered in the
ownership manifest and carry its marker; the only vendored source exception is
the SHA-pinned Bottle acceptance fixture. No existing handwritten file gets a
size exemption. Semantic module extraction is still in progress.

Validation: 12 focused safety/line-count tests and the complete 169-test Python
contract suite pass. The CI size gate remains unsatisfied until every owned
source split is integrated.
