import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

const commands = [
  { name: "careful", description: "Warn before destructive commands", usage: "/careful", source: "skill" as const },
  { name: "deploy", description: "Deploy the project", usage: "/deploy <environment>", source: "project" as const },
  { name: "mcp.github.triage", description: "MCP prompt triage from github", usage: "/mcp.github.triage [JSON object]", source: "mcp" as const },
]

for (const [width, height] of [[110, 32], [80, 24]] as const) {
  test(`command palette and slash completion at ${width}x${height}`, async () => {
    const setup = await createTestRenderer({ width, height, useThread: false, exitOnCtrlC: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        commands,
        availableActions: [{ action: "rewind", unavailable_reason: "Stop the current turn or wait for it to finish." }],
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toMatchSnapshot("palette")

    app.closePicker()
    await setup.mockInput.typeText("/")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toMatchSnapshot("slash")

    await setup.mockInput.typeText("compact keep the API notes")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toMatchSnapshot("slash arguments")
  })
}
