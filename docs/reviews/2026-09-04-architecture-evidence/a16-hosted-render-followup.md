# Hosted mounted-output performance follow-up

The integrated A16 macOS job failed its unchanged mounted-tool-output process-CPU p95 budget: **22.662 ms against 20 ms**. Run `33928471101`, job `101201993996`. Retained raw samples are `/tmp/rw-ci-tui-macos-artifacts/tui-metrics.json.samples.json`; log `/tmp/rw-ci-tui-macos.log`. The 120-frame series had most large spikes in its first half. That pattern alone cannot identify a cause.

Instrumentation of the unchanged production-path fixture measured reducer, app binding and render/await phases separately. One local run measured p95s of 0.036, 6.761 and 2.080 ms respectively. It also identified repeated work independent of output parsing: binding the hidden Tools workspace, updating all 16 tool cards when 15 projections were unchanged, and rewriting context-panel lists/text when their inputs were unchanged.

The real highlighter received four initial requests and no new highlight request during the output deltas: Markdown paragraphs `Result 38` and `Result 39`, and their two TypeScript fences. Process CPU includes asynchronous worker work, so a spike charged during app binding need not be wholly synchronous binding cost. OpenTUI's `getPerformance` counters do not instrument one-shot highlighting; their empty arrays cannot establish the absence of parser CPU. These local observations do not conclusively attribute the earlier hosted failure.

The production changes remove demonstrated work:

- Hidden Tools presentation waits until activation. Theme rebuild and recycle restoration can explicitly populate it once to restore selection/folds/scroll state.
- Tool cards skip unchanged bindings. Their invalidation includes projection identity, width, expansion, elapsed display and workspace-root generation.
- The context panel skips native writes when its actual input references are unchanged; resize continues to recalculate layout.
- Literal single-line Markdown containing only letters, numbers and spaces returns no syntax highlights without starting a worker query. The conservative match excludes indentation, newlines, punctuation, entities and all Markdown syntax. Other Markdown and every code-language request still use the parser.

No benchmark fixture, sample count, warmup, clock, percentile statistic or threshold changed. Alternating local baseline and binding-only runs were mixed, so the binding optimization alone was not called a demonstrated p95 improvement. After the literal-prose optimization, three full smoke runs passed:

| Candidate | Mounted-tool-output p95, milliseconds |
| --- | --- |
| Pre-change A16 local baseline | 10.301, 10.175, 8.444 |
| Binding-only candidate | 12.067, 6.776, 11.638 |
| Final production changes | 6.049, 5.109, 5.682 |

These are shared-host observations, not calibrated release evidence or proof that the hosted job is green. Raw local reports are `/tmp/rw-render-before-{0,1,2}.json.samples.json`, `/tmp/rw-render-after-{0,1,2}.json.samples.json` and `/tmp/rw-render-final-{0,1,2}.json.samples.json`.

Validation used Bun 1.3.14: typecheck; **546 normal TUI tests, 21 snapshots and 12,198 assertions**; three complete five-test smoke runs; release build **82,915,536 bytes** against the unchanged 100,000,000-byte budget; and diff checks. New native tests verify that hidden Tools does no binding, activation shows current retained output with preserved selection, and literal prose renders without a highlight query while Markdown/code requests still delegate. The recycle test now restores Tools state while the conversation remains the visible view. Existing root-generation, elapsed-time, expansion, width, theme and visual tests pass.

Final logs: `/tmp/rw-render-final-tests.log`, `/tmp/rw-render-final-{0,1,2}.log`, `/tmp/rw-render-final-build.log`. Hosted requalification remains required.
