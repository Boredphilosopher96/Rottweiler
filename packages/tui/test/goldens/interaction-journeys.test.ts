import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { emptySessionReader } from "../fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

for (const [width, height] of [[110, 32], [80, 24]] as const) {
  test(`approval and plan keep a usable composer at ${width}x${height}`, async () => {
    const setup = await createTestRenderer({ width, height, useThread: false, exitOnCtrlC: false })
    renderer = setup.renderer
    const initial = createInitialState()
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: {
      ...initial,
      tools: { edit: {
        invocationId: "edit", toolCallId: "edit", turnId: "turn", name: "edit",
        args: { path: "src/main.rs" }, status: "awaiting_approval", capabilities: ["write_filesystem"],
        rationale: "Apply the reviewed correction", diff: null, diffSource: null,
        chunks: toolOutputBuffer([]), display: null, source: null, isError: null,
        callIndex: 0, timing: { kind: "unknown" },
      } },
    } })
    renderer.root.add(app)
    app.composer.value = "Please keep the public API compatible."
    for (let frame = 0; frame < 3; frame++) await setup.renderOnce()
    expect(app.composer.visible).toBeTrue()
    expect(app.interactionPanel.y + app.interactionPanel.height).toBeLessThanOrEqual(app.composer.y)
    expect(setup.captureCharFrame()).toMatchSnapshot("approval")
    app.setState({ ...initial, mode: "plan", pendingPlan: {
      title: "Fix the request boundary", summary_md: "Keep the public API compatible.",
      steps: [{ description: "Validate request inputs", files_touched: ["src/main.rs"], verification: "cargo test" }],
      open_questions: ["Should empty requests be rejected?"],
    } })
    for (let frame = 0; frame < 3; frame++) await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Files: src/main.rs")
    expect(setup.captureCharFrame()).toContain("Verify: cargo test")
    expect(setup.captureCharFrame()).toContain("Should empty requests be rejected?")
    expect(app.interactionPanel.y + app.interactionPanel.height).toBeLessThanOrEqual(app.composer.y)
    expect(setup.captureCharFrame()).toMatchSnapshot("plan")
  })
}
