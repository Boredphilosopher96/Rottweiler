# MCP Browser

The MCP Browser is the retained full-primary inventory reached through `/mcp`. It derives server, ready, and tool totals from live descriptors and hands management to the existing typed MCP action and input pickers.

## User paths

- `/mcp` opens the retained inventory and issues one `list_mcp_servers` request.
- Enter on a server opens the existing actions picker for review, approval, enable or disable, and confirmed removal.
- Add HTTPS and Add stdio open the existing validated prompt chains.
- Ctrl-R or the Retry row repeats a failed inventory request while cached rows remain visible.
- Escape from a nested MCP picker restores the retained inventory query, selection, viewport, and focus.

## Production proof

Run `.cursor/skills/verify-rottweiler-tui/scripts/verify.sh mcp-browser /tmp/rottweiler-tui-evidence/<run-id>`.

Require `mcp-browser.{txt,ansi,png,json}` at 110 by 32 and `mcp-browser-narrow.{txt,ansi,png,json}` at 72 by 18. Both captures must come from the production renderer, every JSON assertion must pass, and no SVG may exist.

The wide proof pins the full 27-row primary surface, the complete 73-cell left region, divider at column 73, detail container at column 74, derived header totals, contiguous server names, state colors, exact failed message, and review fields only for the selected matching server. The narrow proof hides the divider and detail pane while retaining a compact truthful selected-server summary.

## Invalid evidence

- A directly opened component without the `/mcp` composer path.
- Guessed transport, OAuth, approval-time, capability, context-token, schema-cost, allowlist, retry-server, reauthorization, sandbox, TOON, or server-mode claims.
- Optimistic server state before a correlated inventory event.
- Visible transcript or context cells behind the browser.
- SVG text or individually positioned glyph rendering.

## Focused checks

- `cd packages/tui && bun test test/mcp-browser.test.ts`
- `cd packages/tui && bun test test/app.test.ts -t "MCP"`
- `cd packages/tui && bun test test/visual-harness.test.ts -t "production MCP browser"`
