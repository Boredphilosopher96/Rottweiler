# UI semantic owner split checkpoint

The former panels module now has review, interaction, context/sidebar, and status owners. Shared panel labels have their own pure owner, so the interaction panel does not import a status renderable. The components barrel names only the intended public components/types; the old panels module is deleted.

The reducer retains event ordering, cursor handling, and exhaustive event dispatch. Command acknowledgements, command-result presentation, session projections, shell state, child activity, tool/todo state, and turn/tail state are separate owners. Their public limits are exported from the defining module. The shared UTF-8 truncation helper lives with display-buffer ownership. This extraction preserves the existing retention policy; it does not claim aggregate parent-state retention is solved.

Runtime tests now follow configuration, lifecycle, subscription, fork, and session-switching responsibilities. Reducer tests follow catalog, tools, children, streaming, queries, and delivery responsibilities. All 74 existing runtime/reducer cases are preserved. No numbered source fragments or compatibility barrel remains.

Validation on pinned Bun 1.3.14:

- Typecheck passes.
- Full TUI suite: 560 passed, 20 unchanged snapshots, including all five rendering/input performance gates and the persistent-SSE 2 ms p99 gate.
- Final test-only regrouping: all 51 reducer tests pass; typecheck passes.
- AST comparison of the 40 original panel and 83 original reducer declarations against their new owners confirms unchanged declaration bodies, ignoring export modifiers and formatting.
- Every extracted source/test file is below 1,500 lines. The remaining app.ts composition/interaction migration is still outstanding, so this is not a claim that the repository-wide size gate passes.

This checkpoint is structural. A17 exclusive interaction routing, A16 aggregate parent-state/cache ownership, A09 host mutation settlement, and A08 terminal presentation ownership remain separate production migrations.
