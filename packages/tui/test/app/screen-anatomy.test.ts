import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import { createRottweilerApp, type RottweilerApp } from "../../src/app"
import { createInitialState, type RottweilerState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"
import { statusText } from "../picker-screen"

const models = [
  { id: "openai_codex/gpt-5.6-terra", displayName: "GPT-5.6 Terra", provider: "openai_codex", aliases: [], current: true, available: true,
    status: null, vision: true, thinking: true, toolCalling: true, contextTokens: "272000" },
]

const state: RottweilerState = {
  ...createInitialState(),
  model: "openai_codex/gpt-5.6-terra",
  models,
  sessions: [
    { sessionId: "session-local", title: "Current work", workspaceName: "Rottweiler", model: "openai_codex/gpt-5.6-terra", driverClientId: null, shellActive: false, activity: null },
    { sessionId: "older", title: "Older work", workspaceName: "Rottweiler", model: "openai_codex/gpt-5.6-terra", driverClientId: null, shellActive: false, activity: null },
    { sessionId: "abandoned", title: "", workspaceName: "Rottweiler", model: "openai_codex/gpt-5.6-terra", driverClientId: null, shellActive: false,
      activity: { updatedUnixMs: 0, turnCount: 0, firstPrompt: null, costMicrosUsd: null } },
    { sessionId: "untitled", title: "", workspaceName: "Rottweiler", model: "openai_codex/gpt-5.6-terra", driverClientId: null, shellActive: false, activity: null },
  ],
  queuedControls: [{
    request: { protocol_version: PROTOCOL_VERSION, client_id: "tui-client", request_id: "queued-switch" },
    action: { type: "switch_model", model: "openai_codex/gpt-5.6-terra", provider: "openai_codex" },
    status: "queued",
  }],
  workspaceRoots: { generation: "1", effectiveFromTurn: "0", roots: ["/workspace"] },
}

describe("navigation screen anatomy", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  const screens: readonly (readonly [string, (app: RottweilerApp) => void])[] = [
    ["sessions", app => app.openSessionPicker()],
    ["models", app => app.openModelPicker()],
    ["queued", app => app.openQueuedMessagesPicker()],
    ["workspace", app => app.openWorkspaceRootsPicker()],
    ["errors", app => app.openErrorsPicker()],
    ["usage", app => app.openCostPicker()],
  ]

  for (const [width, height] of [[110, 32], [80, 24]] as const) {
    test(`every generic screen owns the whole primary area at ${width}x${height}`, async () => {
      const setup = await createTestRenderer({ width, height, useThread: false })
      renderer = setup.renderer
      const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: state, onCommand: () => ({ type: "accepted" }) })
      renderer.root.add(app)
      for (const [name, open] of screens) {
        open(app)
        await setup.renderOnce()
        expect(app.picker.visible, name).toBeTrue()
        expect([app.picker.x, app.picker.y, app.picker.width], name).toEqual([0, 0, width])
        expect(app.picker.height, name).toBe(height - app.statusLine.height - app.composer.dockHeight)
        const frame = setup.captureCharFrame().split("\n")
        // Nothing outside the screen but the composer dock and status line.
        expect(frame.slice(0, app.picker.height).join("\n"), name).not.toContain("Describe a task, or press /")
        expect(app.picker.layoutMode, name).toBe(width >= 90 ? "split" : "single")
        expect(app.picker.footer.plainText, name).toMatch(/esc (close|back)$|esc (close|back) · /)
        app.closePicker()
      }
    })
  }

  test("sessions list real sessions only, with display names instead of route ids", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const selected: string[] = []
    const commands: string[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader, initialState: state,
      onCommand: command => { commands.push(command.type); return { type: "accepted" } },
      onSessionSelect: id => { selected.push(id) },
    })
    renderer.root.add(app)
    app.openSessionPicker()
    await setup.renderOnce()
    // A session with no accepted prompt is not listed unless it is the open one.
    expect(app.picker.items.map(item => item.label)).toEqual(["Current work", "Older work", "Untitled"])
    expect(app.picker.selectedItem).toMatchObject({ label: "Current work", marker: "●" })
    const frame = setup.captureCharFrame()
    expect(frame).toContain("GPT-5.6 Terra")
    expect(frame).not.toContain("openai_codex/")
    setup.mockInput.pressKey("n", { ctrl: true })
    expect(commands).toContain("create_session")
    app.picker.selectById("older")
    app.picker.activateSelected()
    expect(selected).toEqual(["older"])
    expect(app.picker.visible).toBeFalse()

    app.openQueuedMessagesPicker()
    expect(app.picker.items.map(item => item.label)).toEqual(["Switch model · GPT-5.6 Terra"])
    expect(app.picker.selectedItem?.primary).toBeUndefined()
  })

  test("status screens swallow typing and show their message in the list", async () => {
    const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, onCommand: () => ({ type: "accepted" }) })
    renderer.root.add(app)
    app.composer.value = "draft"
    app.openErrorsPicker()
    await setup.mockInput.typeText("abc")
    expect(app.picker.mode).toBe("status")
    expect(statusText(app.picker)).toBe("No errors in this session\nFailures are kept here, newest first, until the session ends.")
    expect(app.picker.input.visible).toBeFalse()
    expect(app.composer.value).toBe("draft")
  })
})
