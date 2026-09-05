# Updater reconciliation

A live GitHub recheck during remediation confirmed that the latest main batch remains distinct from stale updater candidates.

| Candidate | Exact head | CI outcome | Cause |
| --- | --- | --- | --- |
| PR47, old npm TUI group |26f9936ff5c1c223043b8229260bc5364d5b0584|[run33931475489 failed](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33931475489)|Changed only packages/tui/package.json; no bun.lock change. Frozen installation failed before test execution.|
| PR48, old npm SDK group |990a6426dcb5f4ef22ab2eaa13a5d4fa5d3845b9|[run33931459274 failed](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33931459274)|Changed only packages/plugin-sdk/package.json; no bun.lock change. Frozen installation failed before test execution.|
| PR49, Actions group |8c71c26c84790ac2306420f0aa26c506fa17850c|[run33931507502 passed](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33931507502)|Normal CI only.|
| PR53, Rust group |89dea018f7353b7feb2ab6c24d4fec5b2372f122|[run33931639347 passed](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33931639347)|Normal CI only.|

Read-only commands used: `gh run list`, `gh run view --log-failed`, `gh pr list`, and `gh api repos/Boredphilosopher96/Rottweiler/pulls/{47,48}/files`. Both failed candidates report `error: lockfile had changes, but lockfile is frozen`. Re-running unchanged heads would repeat the manifest/lock mismatch.

The inventory projection now uses package-ecosystem bun for all four Bun packages and rejects npm entries for those directories. Both old branches were then reconciled with the pinned Bun1.3.14 lockfile generator. Frozen installation passes on each repaired candidate.

- SDK PR48 is now b5743eb47939f124bc88d5ff41d725343e140ed7. All65 SDK tests, typecheck and build passed locally. [CI33943559383](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33943559383) passed every gate at that exact head. The PR merged as1038c95b897fb1f7122a460f66c93bf278d76c77 after the clean merge state and absence of review threads were checked. [Post-merge CI33943937413](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33943937413) also passed at that main commit. The temporary SDK verification worktree and its installed/build outputs were removed after evidence retention.
- TUI PR47 is now33b5376fdbfc8c319f2ef11c5f40b54d46625350. OpenTUI0.5.10 changes pointer selection to inclusive endpoints and adds repeated-click word/line selection. The old tool-card handler confused any selected text with a drag, preventing rapid reopening of full output. The repaired handler tracks press/drag/leave and preserves actual text-selection gestures. Mouse tests target the last intended cell. All546 tests,21 unchanged snapshots,typecheck,compiled build,transport gate and five performance smoke gates passed locally. [CI33943696240](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33943696240) passed every gate. The SDK merge made the TUI branch behind main; merge404560f8844288e1eb2ea6e5775f9960ff2b74d7 incorporates that base without changing the TUI patch. [CI33944428610](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33944428610) passed every gate at the updated exact head. After checking clean mergeability and no review threads, the PR merged ascb3b8d5305b91b4095b4fd54d35d9824fd8f3247. [Post-merge CI33944851087](https://github.com/Boredphilosopher96/Rottweiler/actions/runs/33944851087) also passed at that main commit. Its temporary verification worktree and installed/build outputs were removed after evidence retention.

[Local check receipts](updater-local-checks.json), [raw smoke output](updater-tui-smoke.log) and [smoke metrics](updater-tui-smoke-metrics.json) are retained. Smoke statistics and ceilings were unchanged. These local checks do not establish controlled performance baselines or native Linux qualification. No PR comments or review messages were posted.

A successful Dependabot Updates job after main3ef57e9 does not prove a fresh native Bun-generated candidate maintained every lockfile. The old candidates are repaired; the new updater's next generated candidate still needs frozen-install and exact-head CI evidence.

`gh api repos/Boredphilosopher96/Rottweiler/actions/runners` still returns total_count0 and an empty runners list. Protected native soaks remain unqualified.
