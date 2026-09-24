import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import { createStreamingTail } from "../../src/state/model"
import { emptySessionReader } from "../fixtures/history"
import { options } from "../picker-screen"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

for (const [width, height] of [[110, 32], [80, 24]] as const) {
  test(`navigation covers the transcript and preserves the composer at ${width}x${height}`, async () => {
    const setup = await createTestRenderer({ width, height, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: {
      ...createInitialState(), streamingTail: createStreamingTail({ turnId: "1",
        text: Array.from({ length: 40 }, () => "TRANSCRIPT-CANARY ".repeat(10)).join("\n"),
        thinking: "", citations: [], toolInvocationIds: [], finished: null }),
    } })
    renderer.root.add(app)
    app.composer.value = "Retained steering draft"
    const screens = [
      () => app.openCommandPicker(), () => app.openModelPicker(), () => app.openProviderPicker(),
      () => app.openSessionPicker(), () => app.openSubagentPicker(), () => app.openContextPicker(),
      () => app.openCostPicker(), () => app.openErrorsPicker(), () => app.openPermissionPicker(),
      () => app.openPermissionModePicker(), () => app.openTrustPicker(), () => app.openAttachmentPicker(),
      () => app.openWorkspaceRootsPicker(), () => app.openExportSessionPicker(),
      () => app.openQueuedMessagesPicker(), () => app.openKeyboardHelpPicker(),
      () => app.openThemePicker(), () => app.openSettingsPicker(), () => app.openMcpPicker(),
      async () => { app.composer.value = "/skills"; await app.composer.submit(); app.composer.value = "Retained steering draft" },
    ]
    for (const open of screens) {
      app.closePicker()
      await open()
      await setup.renderOnce()
      const screen = [app.commandPalette, app.mcpBrowser, app.settingsBrowser, app.themeBrowser, app.agentsBrowser, app.skillsBrowser, app.picker].find(item => item.visible)!
      expect(screen).toBeDefined()
      expect(screen.x).toBe(0)
      expect(screen.y).toBe(0)
      expect(screen.width).toBe(width)
      expect(screen.height).toBe(app.composer.y)
      expect(setup.captureCharFrame()).not.toContain("TRANSCRIPT-CANARY")
      expect(setup.captureCharFrame()).toContain("Retained steering draft")
    }
  })
}

for (const [width, height] of [[110, 32], [80, 24]] as const) {
  test(`finished child result remains inspectable at ${width}x${height}`, async () => {
    const setup = await createTestRenderer({ width, height, useThread: false })
    renderer = setup.renderer
    let list: Extract<import("../../src/protocol").ClientCommand, { type: "list_subagents" }> | undefined
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, sessionId: "parent", initialState: {
      ...createInitialState(), subagentOrder: ["child"], subagents: { child: {
        projectionId: "child", subagentId: "child", parentTurnId: "1", task: "Inspect code", spawnedAtMs: null,
        status: "completed", childSessionId: "child-session", lastChildSequence: "4", activity: null,
        summary: "Verified the change and retained the child result.", touchedFileCount: 2, diffArtifactId: null,
        cost: { kind: "monetary", currency: "USD", amount_micros: "12500" },
      } },
    }, onCommand(command) { if (command.type === "list_subagents") list = command; return { type: "accepted" } } })
    renderer.root.add(app)
    app.openSubagentPicker()
    app.handleEvent({ type: "subagents_listed", meta: { ...list!.meta, emitted_at: "2026-09-16T00:00:00Z" }, session_id: "parent",
      subagents: [{ subagent_id: "child", child_session_id: "child-session", task: "Inspect code", agent: "reviewer", model: "coding", isolation: "shared", activity: "idle" }] })
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Verified the change and retained the child result.")
    expect(setup.captureCharFrame()).toContain("USD 0.0125")
    expect(app.agentsBrowser.sectionLabels).toEqual(["Finished"])
    expect(app.agentsBrowser.height).toBe(app.composer.y)
    app.agentsBrowser.activateSelected()
    expect(app.picker.screenTitle).toContain("completed")
    expect(options(app.picker).map(option => option.value)).toEqual(["view", "message", "close"])
  })
}
