import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"

import { createRottweilerApp } from "../../src/app"

let renderer: TestRenderer | undefined

afterEach(() => {
  renderer?.destroy()
  renderer = undefined
})

test("Vim mode is visible and retains the normal-mode draft", async () => {
  const setup = await createTestRenderer({ width: 84, height: 16, useThread: false })
  renderer = setup.renderer
  const app = createRottweilerApp(renderer, { keybindings: { preset: "vim" } })
  renderer.root.add(app)
  setup.mockInput.pressKey("i")
  await setup.mockInput.typeText("review this draft")
  setup.mockInput.pressEscape()
  await Bun.sleep(30)
  await setup.renderOnce()

  expect(
    JSON.stringify({
      frame: setup.captureCharFrame(),
      styledSpanCount: setup.captureSpans().lines.flat().length,
    }),
  ).toMatchSnapshot()
})
