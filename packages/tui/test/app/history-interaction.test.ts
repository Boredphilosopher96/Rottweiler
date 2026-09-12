import { CliRenderEvents, type Selection } from "@opentui/core"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import {
  createRottweilerApp
} from "../../src/app"
import {
  kennelTheme
} from "../../src/theme"
import { waitForHistory, emptySessionReader, sessionReaderFor, conversationItem, toolItem } from "../fixtures/history"
import { rgba } from "./fixtures"

describe("Rottweiler history-interaction", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("selects and toggles a transcript block from the focused composer, then clears on typing", async () => {
    const setup = await createTestRenderer({
      width: 88,
      height: 18,
      useThread: false,
      kittyKeyboard: true,
    })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: sessionReaderFor([toolItem(1, "read", '{"path":"keyboard.txt"}', "keyboard output")]),

    })
    renderer.root.add(app)
    await setup.flush()
    const block = app.transcript.mountedCards.get("1")

    setup.mockInput.pressArrow("down", { ctrl: true })
    expect(app.transcript.selectedBlockId).toBe("tool:invocation-1")
    expect(block?.header.bg.toInts()).toEqual(rgba(kennelTheme.backgroundElement))
    expect(renderer.currentFocusedRenderable?.id).toBe("composer-editor")

    setup.mockInput.pressKey(" ", { ctrl: true })
    expect(block?.markdown.visible).toBeTrue()
    expect(renderer.currentFocusedRenderable?.id).toBe("composer-editor")

    await setup.mockInput.typeText("x")
    expect(app.transcript.selectedBlockId).toBeNull()
    expect(block?.header.bg.toInts()).toEqual(rgba(kennelTheme.background))
  })

  test("copies a completed mouse selection once, clears it, and restores composer focus", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const copied: string[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: sessionReaderFor([conversationItem(1, "assistant", "Selectable transcript text")]),

      textClipboard: {
        async writeText(value) {
          copied.push(value)
        },
      },
    })
    renderer.root.add(app)
    await setup.flush()
    const card = [...app.transcript.mountedCards.values()][0]
    expect(card?.markdown.selectable).toBeTrue()

    await setup.mockMouse.pressDown(
      card!.markdown.x + 2 + 1,
      card!.markdown.y,
    )
    expect(renderer.getSelection()).not.toBeNull()
    await setup.mockMouse.emitMouseEvent(
      "drag",
      card!.markdown.x + 2 + "Selectable".length - 1,
      card!.markdown.y,
    )
    await setup.mockMouse.release(
      card!.markdown.x + 2 + "Selectable".length - 1,
      card!.markdown.y,
    )
    await setup.waitFor(() => copied.length === 1)
    expect(copied[0]).toBe("electable")
    expect(renderer.getSelection()).toBeNull()
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
    expect(app.banner.plainText).toBe("Copied to clipboard")

    // Composer selections use the same completed-selection path without
    // handing keyboard focus away from the editor.
    app.composer.value = "composer draft"
    await setup.flush()
    await setup.mockMouse.drag(
      app.composer.editor.x + 1,
      app.composer.editor.y,
      app.composer.editor.x + "composer".length - 1,
      app.composer.editor.y,
    )
    await setup.waitFor(() => copied.length === 2)
    expect(copied[1]).toBe("omposer")
    expect(renderer.getSelection()).toBeNull()
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
    expect(app.banner.plainText).toBe("Copied to clipboard")
  })

  test("scrolls the transcript with PageUp in standard mode without blurring the composer", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: sessionReaderFor(Array.from({ length: 40 }, (_, index) => conversationItem(index + 1, "assistant", `Retained line ${index}`))),

    })
    renderer.root.add(app)
    await setup.flush()
    app.transcript.scrollTo(app.transcript.scroller.scrollHeight)
    app.composer.focus()
    const before = app.transcript.scroller.scrollTop

    setup.mockInput.pressKey("\x1b[5~")
    await setup.flush()

    expect(app.transcript.scroller.scrollTop).toBeLessThan(before)
    expect(app.composer.editor.focused).toBeTrue()
  })

  test("restores a source anchor after destroying the renderer and loading a differently wrapped history window", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const items = Array.from({ length: 1000 }, (_, index) => conversationItem(index + 1, "assistant", `Retained line ${index} with wrapping text. `.repeat(5)))
    const reader = sessionReaderFor(items)
    const app = createRottweilerApp(renderer, { sessionReader: reader })
    renderer.root.add(app)
    await setup.flush()
    app.transcript.scrollTo(0)
    await setup.flush()
    app.transcript.setScrollOffset(5)
    await setup.flush()
    app.composer.value = "unfinished prompt"
    const saved = app.recycleState()
    if (saved === null) throw new Error("expected a restorable draft")
    expect(saved.history.following).toBe(false)
    expect(saved.history.anchor).not.toBeNull()
    expect(Number(saved.history.anchor?.id)).toBeLessThan(32)
    renderer.root.remove(app)
    app.destroyRecursively()
    renderer.destroy()

    const recreated = await createTestRenderer({ width: 45, height: 16, useThread: false })
    renderer = recreated.renderer
    const replacement = createRottweilerApp(renderer, { sessionReader: reader })
    renderer.root.add(replacement)
    replacement.restoreRecycleState(saved)
    await recreated.flush()
    replacement.applyPendingRecycleScroll()
    await recreated.flush()

    expect(replacement.composer.value).toBe("unfinished prompt")
    expect(replacement.transcript.captureHistoryViewport()).toEqual(saved.history)
    expect(replacement.transcript.mountedEntryCount).toBeLessThanOrEqual(16)
  })

  test("does not clear a newer selection when an older clipboard write finishes", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const copied: string[] = []
    const complete: Array<() => void> = []
    const app = createRottweilerApp(renderer, {
      sessionReader: sessionReaderFor([conversationItem(1, "assistant", "First selectable value")]),

      textClipboard: {
        writeText(value) {
          copied.push(value)
          return new Promise<void>((resolve) => complete.push(resolve))
        },
      },
    })
    renderer.root.add(app)
    await setup.flush()
    const card = [...app.transcript.mountedCards.values()][0]!

    await setup.mockMouse.drag(
      card.markdown.x + 2 + 1,
      card.markdown.y,
      card.markdown.x + 2 + "First".length,
      card.markdown.y,
    )
    await setup.waitFor(() => copied.length === 1)

    app.composer.value = "Second selectable value"
    await setup.flush()
    await setup.mockMouse.drag(
      app.composer.editor.x + 1,
      app.composer.editor.y,
      app.composer.editor.x + "Second".length,
      app.composer.editor.y,
    )
    await setup.waitFor(() => copied.length === 2)
    const newerSelection = renderer.getSelection()
    expect(newerSelection).not.toBeNull()

    complete[0]?.()
    await Bun.sleep(0)
    expect(renderer.getSelection()).toBe(newerSelection)

    complete[1]?.()
    await Bun.sleep(0)
    expect(renderer.getSelection()).toBeNull()
    expect(app.banner.plainText).toBe("Copied to clipboard")
  })

  test("mounted command rows reject malformed JSON and redact command secrets", async () => {
    const messages = [
      '{"api_key":"must-not-render","nested":{"access_token":"also-secret"}}',
      '{"machine_local_path":"/private/repo",',
    ]
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: sessionReaderFor(messages.map((text, index) => ({
      id: String(index + 1), ordinal: String(index), revision: String(index + 1), agent_turn: null,
      content: { type: "command", name: "extension", message: { text, format: "text", complete: true,
        source: { sequence: String(index + 1), selector: { type: "command_message" } } } },
    }))) })
    renderer.root.add(app)
    await waitForHistory(setup, () => app.transcript.mountedEntryCount === 2)
    const results = [...app.transcript.mountedCards.values()].sort((a, b) => Number(a.item.ordinal) - Number(b.item.ordinal)).map(row => row.markdown.content)
    expect(results[0]).toContain("Api key: [redacted]")
    expect(results[0]).toContain("Access token: [redacted]")
    expect(results[0]).not.toContain("must-not-render")
    expect(results[0]).not.toContain("also-secret")
    expect(results[1]).toBe("_Command returned structured details that could not be displayed safely._")
    expect(results[1]).not.toContain("machine_local_path")
    expect(results[1]).not.toContain("/private/repo")
  })

  test("reports clipboard failures without mislabeling non-transcript selections", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      textClipboard: {
        async writeText() {
          throw new Error("clipboard unavailable")
        },
      },
    })
    renderer.root.add(app)

    renderer.emit(CliRenderEvents.SELECTION, {
      selectedRenderables: [app.composer.editor],
      getSelectedText: () => "composer draft",
    } as unknown as Selection)
    await Bun.sleep(0)
    expect(app.state.errors.at(-1)).toMatchObject({
      code: "selection_copy_failed",
      message: "Couldn't copy the selected text to the clipboard.",
    })
  })
})
