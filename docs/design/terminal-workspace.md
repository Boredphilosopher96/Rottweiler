# Terminal workspace

The terminal workspace presents conversation, tools and task controls through native OpenTUI components. The engine owns durable facts and permitted actions. The client owns visual hierarchy, selection, focus, transient drafts and navigation.

## Workspace anatomy

The primary area contains the conversation or an active workspace screen. A composer dock holds editable input and pending attachments. Status and footer controls expose the session's actual mode, driver state and available shortcuts.

Conversation hierarchy uses user gutters, assistant markers, a reasoning bar and child-agent brackets. Routine content stays unboxed. Text is rendered as complete native runs so wrapping and selection follow terminal cells. Historical content uses the [paged transcript](paged-transcript-client.md); live text and tool activity have a separate tail.

Tools remain identifiable through their host invocation identity. Their headers and output actions distinguish a click from a drag, allowing selectable output without accidental activation. Expanded reasoning and output use bounded previews; historical full-content actions open the paged document reader.

## Browsers and focused interactions

Command palette, theme, settings and MCP screens share list/detail anatomy: a filter, a selected row, a detail region and context-specific actions. Narrow layouts remove the secondary detail region before compressing the primary list. Anchored slash completion and short choices use the compact picker near the composer.

Session Review gives the diff the primary area and uses a 37-column decision rail when the terminal is at least 110 columns wide and the primary area is at least 12 rows tall. Below those dimensions the rail is hidden. File decisions remain bound to the exact proposal and fingerprint; truncated changes expose only decisions authorized by their typed review state.

The Tools workspace presents retained activity, output access and event-derived timing. Context, plan and interaction panels consume typed engine state. Missing, loading, stale and failed queries have distinct presentation. No screen manufactures repository facts, provider reachability, elapsed time or cost attribution from display text.

API dollars, AI credits and unpriced subscription usage are separate quantities. Completed-turn totals come from durable turn summaries. An accounting receipt is not another visible turn charge.

## Input and focus ownership

`PickerController` owns compact-picker selection, positioning, query and close reason. A dismissal may return to its parent browser. A session-scope change closes the interaction without reopening that browser. Modal entry and exit coordinate native focus with the configured Vim mode.

`ProviderUiController` owns provider selection, onboarding, credential submission and authorization attempts. Async completions carry an operation identity and cannot mutate a replaced interaction after session change or destruction. Credential submission itself remains owned by the command transport until settled.

`McpUiController` owns the server browser and its multi-step command drafts. Text prompts explicitly declare whether empty input submits and carry a UTF-8 byte limit. Native character limits permit any value within that byte budget. MCP environment drafts replace repeated keys, admit at most 128 entries and 64 KiB of key/value bytes, and transfer ownership into the submitted command. Closing the workflow clears its draft.

Composer text and attachments are editable client state. Failed submission retains recoverable input. Secret prompts and process-bound interactions are excluded from renderer handoff; they do not enter persisted conversation history merely because a component captures its view state.

## Session and request boundaries

Every query result is correlated to its client, request and applicable session. A successful HTTP command response must carry a valid typed reply matching the command class. Invalid response bodies terminate that subscription attempt as protocol errors rather than entering an automatic retry loop.

Session changes invalidate scoped interaction callbacks and query ownership. Child navigation uses the child session's history capability. Historical replay is read-only: displaying an action does not bypass driver, replay, approval or foreground-shell checks.

Mutation failures are visible through the normal error presentation. A local selection or optimistic focus change does not imply that the engine accepted an operation. Workspace-root changes, permissions, review decisions and provider credentials use their typed command contracts.

## Rendering and state boundaries

Native components own layout and terminal rendering; feature controllers own their workflows. Themes provide semantic roles shared by transcript, browsers, panels and footer controls. Preview cancellation restores the selected theme rather than persisting an unconfirmed choice.

`AppClientState` describes bounded in-memory renderer handoff. Capture includes editable composer state and supported selection/focus state. Replacement waits when an active interaction cannot be captured safely or the payload exceeds the private handoff limit. This handoff is distinct from durable session history and does not persist credentials to disk.

Production-renderer tests cover terminal cells, wide and narrow layout, native mouse targeting, keyboard focus, replay guards and request correlation. Performance checks measure mounted output, live streams and input through the real OpenTUI path; visual snapshots alone do not establish responsiveness.

## Picker interaction lifetime

The picker controller owns the active route and its interaction lease together. Opening or replacing a route retires the previous lease; refreshing its data preserves it. Selection callbacks and custom setting/permission prompts act only while their captured lease is active. Renderer destruction retires the lease without reopening a parent browser. This keeps delayed callbacks from acting on a different dialog or session.

Each interaction can own one retirement cleanup. The controller publishes the next route before invoking that cleanup. Theme previews use it to restore the original palette when cancelled or replaced. Theme confirmation admits one pending save, freezes preview changes while saving, and applies its result only while that interaction remains active. A late success or failure cannot dismiss another dialog.
