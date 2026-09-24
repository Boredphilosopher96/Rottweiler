import { bold, fg, StyledText } from "@opentui/core"
import {
  createTestRenderer,
  type TestRenderer
} from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { ListDetailRenderable, PickerScreenRenderable } from "../../src/components"
import { kennelTheme } from "../../src/theme"
import { listDetailRows } from "./fixtures"
import { statusText } from "../picker-screen"

describe("list-detail components", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("uses presentation-owned empty copy for request states", () => {
    return createTestRenderer({ width: 80, height: 18, useThread: false }).then(({ renderer: testRenderer }) => {
      renderer = testRenderer
      const list = new ListDetailRenderable<string>(testRenderer, kennelTheme)
      testRenderer.root.add(list)
      list.open({
        title: "SETTINGS   /settings",
        query: "",
        rows: [],
        selectedId: null,
        status: "Loading settings",
        emptyCopy: "Loading settings",
      }, () => { })
      expect(list.detail.plainText).toBe("Loading settings")
      expect(list.compactDetail.plainText).toBe("Loading settings")
    })
  })

  test("uses the exact 52/1/51 split at the 110-column design size", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const list = new ListDetailRenderable<string>(renderer, kennelTheme)
    renderer.root.add(list)
    list.open({ title: "Command palette", query: "", rows: listDetailRows, selectedId: "compact", status: "24 commands" }, () => { })
    await setup.renderOnce()

    expect(list.width).toBe(108)
    expect(list.height).toBe(25)
    expect(list.listPane.width).toBe(52)
    expect(list.divider.width).toBe(1)
    expect(list.detailPane.width).toBe(51)
    expect(list.divider.x).toBe(list.listPane.x + 52)
  })

  test("supports a 34-cell list and complete styled theme rows and detail without changing defaults", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const list = new ListDetailRenderable<string>(renderer, kennelTheme, {
      splitListWidth: 34,
      inputPlaceholder: "Filter themes…",
      emptyCopy: "No matching themes",
      renderRow(row, selected) {
        return new StyledText([
          fg(kennelTheme.primary)(selected ? "▸ " : "  "),
          bold(fg(kennelTheme.text)(row.label)),
          fg(kennelTheme.background)("██"),
          fg(kennelTheme.primary)("██"),
        ])
      },
      renderDetail(row) {
        return new StyledText([
          bold(fg(kennelTheme.text)(row.detail.title)),
          fg(kennelTheme.textMuted)("  dark · 52 roles resolved · live sample"),
          fg(kennelTheme.primary)("\n▌ you"),
        ])
      },
    })
    renderer.root.add(list)
    list.open({
      title: "THEME   34 themes   /theme",
      query: "",
      rows: listDetailRows,
      selectedId: "compact",
      status: "34 themes · dark · 0 custom",
    }, () => { })
    await setup.renderOnce()

    expect(list.listPane.width).toBe(34)
    expect(list.divider.width).toBe(1)
    expect(list.detailPane.width).toBe(69)
    expect(list.divider.x).toBe(list.listPane.x + 34)
    expect(list.input.placeholder).toBe("Filter themes…")
    expect((list.rowViews[1]?.content as StyledText).chunks.map((chunk) => chunk.text)).toEqual([
      "▸ ", "Compact context", "██", "██",
    ])
    expect((list.detail.content as StyledText).chunks.map((chunk) => chunk.text)).toEqual([
      "Compact context", "  dark · 52 roles resolved · live sample", "\n▌ you",
    ])

    list.scrollViewport(5)
    expect(list.scrollOffset).toBe(5)
    list.restoreViewport(2)
    expect(list.scrollOffset).toBe(2)
    list.resizeForTerminal(80, 18)
    expect(list.layoutMode).toBe("split")
    expect(list.listPane.width).toBe(34)
    list.resizeForTerminal(79, 18)
    expect(list.layoutMode).toBe("single")
    expect(list.divider.visible).toBeFalse()
    expect(list.detailPane.visible).toBeFalse()

    list.open({
      title: "THEME   34 themes   /theme",
      query: "none",
      rows: [],
      selectedId: null,
      status: "0 of 34 themes · dark · 0 custom",
    }, () => { })
    expect(list.detail.plainText).toBe("No matching themes")
    expect(list.compactDetail.plainText).toBe("No matching themes")
  })

  test("lays the theme variant over the complete primary surface", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const list = new ListDetailRenderable<string>(renderer, kennelTheme, {
      surfaceLayout: "primary",
      splitListWidth: 33,
      splitMinWidth: 100,
      compactMinHeight: 8,
    })
    renderer.root.add(list)
    list.open({
      title: "THEME   34 themes   /theme",
      query: "",
      rows: listDetailRows,
      selectedId: "compact",
      status: "34 themes · dark · 0 custom",
    }, () => { })
    await setup.renderOnce()

    expect(list.x).toBe(0)
    expect(list.y).toBe(0)
    expect(list.width).toBe(110)
    expect(list.height).toBe(27)
    expect(list.listPane.x).toBe(1)
    expect(list.listPane.width).toBe(33)
    expect(list.divider.x).toBe(34)
    expect(list.divider.y).toBe(0)
    expect(list.divider.height).toBe(27)
    expect(list.detailPane.x).toBe(35)
    expect(list.detailPane.y).toBe(0)
    expect(list.detailPane.width).toBe(74)
    expect(list.footer.width).toBe(33)

    list.resizeForTerminal(99, 32, 27)
    await setup.renderOnce()
    expect(list.layoutMode).toBe("single")
    expect(list.listPane.width).toBe(97)
    expect(list.detailPane.visible).toBeFalse()
    expect(list.compactDetail.visible).toBeTrue()
    expect(list.compactDetail.plainText).toBe("Compact the conversation context")

    list.resizeForTerminal(100, 32, 27)
    await setup.renderOnce()
    expect(list.layoutMode).toBe("split")
    expect(list.listPane.width).toBe(33)
    expect(list.detailPane.x).toBe(35)
    expect(list.detailPane.width).toBe(64)

    list.resizeForTerminal(64, 14, 9)
    await setup.renderOnce()
    expect(list.layoutMode).toBe("single")
    expect(list.compactDetail.visible).toBeTrue()
    expect(list.compactDetail.plainText).toBe("Compact the conversation context")
  })

  test("updates detail with selection and keeps scrolling independent", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const list = new ListDetailRenderable<string>(renderer, kennelTheme)
    renderer.root.add(list)
    list.open({ title: "Command palette", query: "", rows: listDetailRows, selectedId: "compact", status: "24 commands" }, () => { })
    await setup.renderOnce()

    expect(list.detail.plainText).toContain("Compact the conversation context")
    list.moveSelection(1)
    expect(list.selectedId).toBe("rewind")
    expect(list.detail.plainText).toContain("Choose from completed user turns")
    const selected = list.selectedId
    list.scrollViewport(1)
    expect(list.selectedId).toBe(selected)
    expect(list.scrollOffset).toBe(1)
  })

  test("activates the exact visible mouse row and styles labels as complete runs", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const actions: string[] = []
    const list = new ListDetailRenderable<string>(renderer, kennelTheme)
    renderer.root.add(list)
    list.open({ title: "Command palette", query: "re", rows: listDetailRows, selectedId: "compact", status: "24 commands" }, (action) => actions.push(action))
    await setup.renderOnce()

    const styled = list.rowViews[2]?.content
    expect(typeof styled).toBe("object")
    expect((styled as { chunks: readonly { text: string }[] }).chunks.map((chunk) => chunk.text)).toEqual([
      "  ", "Re", "wind ", "to", " a turn",
    ])
    await setup.mockMouse.click(list.listPane.x + 2, list.listPane.y + 3)
    expect(actions).toEqual(["command-0"])
  })

  test("does not activate when mouse down and mouse up land on different rows", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const actions: string[] = []
    const list = new ListDetailRenderable<string>(renderer, kennelTheme)
    renderer.root.add(list)
    list.open({ title: "Command palette", query: "", rows: listDetailRows, selectedId: "rewind", status: "24 commands" }, (action) => actions.push(action))
    await setup.renderOnce()

    await setup.mockMouse.pressDown(list.listPane.x + 2, list.listPane.y + 1)
    expect(list.selectedId).toBe("compact")
    await setup.mockMouse.release(list.listPane.x + 2, list.listPane.y + 2)

    expect(list.selectedId).toBe("compact")
    expect(actions).toEqual([])
  })

  test("uses one pane at narrow widths without duplicating the selected description", async () => {
    const setup = await createTestRenderer({ width: 72, height: 18, useThread: false })
    renderer = setup.renderer
    const list = new ListDetailRenderable<string>(renderer, kennelTheme)
    renderer.root.add(list)
    list.resizeForTerminal(72, 18)
    list.open({ title: "Command palette", query: "", rows: listDetailRows, selectedId: "compact", status: "24 commands" }, () => { })
    await setup.renderOnce()

    expect(list.layoutMode).toBe("single")
    expect(list.divider.visible).toBeFalse()
    expect(list.detailPane.visible).toBeFalse()
    expect(setup.captureCharFrame().match(/Compact the conversation context/g)).toHaveLength(1)
  })

  test("filters generic screen rows and shows no-match copy in the list, not as a row", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const selected: string[] = []
    const picker = new PickerScreenRenderable<string>(renderer, kennelTheme)
    renderer.root.add(picker)
    picker.resizeForTerminal(80, 18, 14)
    picker.present("choices:", "CHOICES", [
      { id: "section", label: "Letters", description: "", sectionHeader: true, value: "" },
      { id: "alpha", label: "Alpha", description: "First", value: "alpha" },
      { id: "beta", label: "Beta", description: "Second", value: "beta" },
    ], (item) => { selected.push(item.value) }, {}, false, "")

    expect(picker.sectionLabels).toEqual(["Letters"])
    await setup.mockInput.typeText("bet")
    expect(picker.items.map(item => item.id)).toEqual(["beta"])
    expect(picker.sectionLabels).toEqual([])
    await setup.mockInput.typeText("zzz")
    expect(picker.items).toEqual([])
    expect(statusText(picker)).toBe("No matches for “betzzz”")
    picker.activateSelected()
    expect(selected).toEqual([])
  })

  test("lists screen chords in a state-aware footer and runs them for the selected row", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const ran: string[] = []
    const picker = new PickerScreenRenderable<string>(renderer, kennelTheme)
    renderer.root.add(picker)
    picker.resizeForTerminal(110, 32, 26)
    picker.present("sessions:", "SESSIONS", [
      { id: "one", label: "One", hint: "GPT-5", marker: "●", description: "first", value: "one" },
      { id: "two", label: "Two", description: "second", primary: null, value: "two" },
    ], (item) => { ran.push(`enter:${item.value}`) }, {
      primary: "resume",
      back: () => { ran.push("back") },
      keys: [{ stroke: "ctrl+r", label: "rename", available: item => item?.value === "one", run: item => { ran.push(`rename:${item?.value}`) } }],
    }, false, "")
    await setup.renderOnce()

    expect(picker.footer.plainText).toBe("⏎ resume · ctrl+r rename · esc back")
    expect(setup.captureCharFrame()).toMatch(/› ● One\s+GPT-5/)
    setup.mockInput.pressKey("r", { ctrl: true })
    expect(ran).toEqual(["rename:one"])
    picker.moveSelection(1)
    expect(picker.footer.plainText).toBe("esc back")
    setup.mockInput.pressKey("r", { ctrl: true })
    picker.activateSelected()
    expect(ran).toEqual(["rename:one"])
    expect(picker.back).toBeDefined()
  })

  test("anchors a composer list above a row without a filter, detail, or footer", async () => {
    const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
    renderer = setup.renderer
    const picked: string[] = []
    const picker = new PickerScreenRenderable<string>(renderer, kennelTheme)
    renderer.root.add(picker)
    picker.present("files:", "@ files", [
      { id: "src/app.ts", label: "src/app.ts", hint: "file", description: "", primary: "attach", value: "src/app.ts" },
      { id: "src/", label: "src/", hint: "dir", description: "", primary: "open", value: "src/" },
    ], (item) => { picked.push(item.value) }, { primary: "attach" }, true, "app")
    picker.resizeInline(80, 17, picker.desiredInlineRows)
    await setup.renderOnce()

    expect(picker.layoutMode).toBe("inline")
    expect(picker.input.visible).toBeFalse()
    expect(picker.detailPane.visible).toBeFalse()
    expect(picker.footer.visible).toBeFalse()
    expect(picker.heading.plainText).toContain("⏎/tab attach")
    setup.mockInput.pressTab()
    expect(picked).toEqual(["src/app.ts"])
  })

  test("text prompts own printable input, byte limits, and Enter", async () => {
    const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
    renderer = setup.renderer
    const submitted: string[] = []
    const picker = new PickerScreenRenderable<string>(renderer, kennelTheme)
    renderer.root.add(picker)
    picker.openTextPrompt({ title: "RENAME", placeholder: "title", maxBytes: 4, empty: "reject",
      detail: "The new title", onSubmit: value => { submitted.push(value) } }, () => {})
    await setup.mockInput.typeText("abcdef")
    expect(picker.input.value).toBe("abcd")
    expect(picker.footer.plainText).toBe("⏎ save · esc back")
    expect(statusText(picker)).toBe("The new title")
    setup.mockInput.pressEnter()
    expect(submitted).toEqual(["abcd"])
  })
})
