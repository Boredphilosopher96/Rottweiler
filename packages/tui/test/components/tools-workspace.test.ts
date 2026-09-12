import {
  createTestRenderer,
  type TestRenderer
} from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { ToolsWorkspaceRenderable } from "../../src/components"
import { kennelTheme } from "../../src/theme"
import { toolsActivity, toolsWorkspaceModel } from "./fixtures"

describe("tools-workspace components", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("retains keyed Tools rows through lifecycle, selection, and user folding", async () => {
    const setup = await createTestRenderer({ width: 110, height: 27, useThread: false })
    renderer = setup.renderer
    const opened: string[] = []
    const workspace = new ToolsWorkspaceRenderable(renderer, kennelTheme, {
      onOpenToolOutput(toolCallId) {
        opened.push(toolCallId)
      },
    })
    renderer.root.add(workspace)
    workspace.resizeForTerminal(110, 27)

    const liveFirst = toolsActivity("stable-first", 12, "running", 3)
    const liveSecond = toolsActivity("stable-second", 4, "running", 2)
    workspace.update(toolsWorkspaceModel([liveFirst, liveSecond]))
    await setup.renderOnce()

    const firstRow = workspace.rowForKey("tool:stable-first")
    const secondRow = workspace.rowForKey("tool:stable-second")
    expect(firstRow).toBeDefined()
    expect(secondRow).toBeDefined()
    expect(firstRow?.expanded).toBeTrue()
    const firstHeader = firstRow?.header.content
    const secondHeader = secondRow?.header.content
    workspace.update(toolsWorkspaceModel([{ ...liveFirst }, { ...liveSecond }]))
    expect(firstRow?.header.content).toBe(firstHeader)
    expect(secondRow?.header.content).toBe(secondHeader)

    workspace.selectNextBlock()
    workspace.selectNextBlock()
    expect(workspace.selectedRowKey).toBe("tool:stable-second")
    firstRow?.expand(false)

    workspace.update(toolsWorkspaceModel([
      { ...liveFirst, outcome: { kind: "awaiting_approval", label: "approval needed" } },
      { ...liveSecond, outcome: { kind: "succeeded", label: "Completed" } },
    ]))
    workspace.update(toolsWorkspaceModel([
      { ...liveFirst, outcome: { kind: "succeeded", label: "Completed" } },
      { ...liveSecond, outcome: { kind: "succeeded", label: "Completed" } },
    ]))
    await setup.renderOnce()

    expect(workspace.rowForKey("tool:stable-first")).toBe(firstRow)
    expect(workspace.rowForKey("tool:stable-second")).toBe(secondRow)
    expect(firstRow?.expanded).toBeFalse()
    expect(workspace.selectedRowKey).toBe("tool:stable-second")

    firstRow?.expand(true)
    await setup.renderOnce()
    expect(firstRow?.output.selectable).toBeTrue()
    expect(firstRow?.getChildren().filter((child) => child.id.includes("output"))).toHaveLength(1)
    expect(firstRow?.marker.visible).toBeTrue()
    await setup.mockMouse.click(firstRow!.marker.x, firstRow!.marker.y)
    expect(opened).toEqual(["stable-first"])
  })

  test("renders foreground shell hidden lines as a non-actionable marker", async () => {
    const setup = await createTestRenderer({ width: 70, height: 12, useThread: false })
    renderer = setup.renderer
    const opened: string[] = []
    const workspace = new ToolsWorkspaceRenderable(renderer, kennelTheme, {
      onOpenToolOutput(toolCallId) {
        opened.push(toolCallId)
      },
    })
    renderer.root.add(workspace)
    workspace.resizeForTerminal(70, 12)
    workspace.update(toolsWorkspaceModel([{
      kind: "foreground_shell",
      key: "shell:foreground-current",
      shellId: "foreground-current",
      command: "bun test",
      active: true,
      status: null,
      output: {
        kind: "text",
        text: Array.from({ length: 8 }, (_, index) => `shell-${index + 5}`).join("\n"),
        retainedLineCount: 12,
        visibleLineCount: 8,
        hiddenRetainedLineCount: 4,
        window: "tail",
        sourceTruncated: false,
      },
    }]))
    await setup.renderOnce()

    const shellRow = workspace.rowForKey("shell:foreground-current")
    expect(shellRow?.marker.visible).toBeTrue()
    expect(shellRow?.marker.plainText).toBe("… 4 more retained lines")
    expect(shellRow?.marker.plainText).not.toContain("view all")
    expect(shellRow?.openOutput()).toBeFalse()
    await setup.mockMouse.click(shellRow!.marker.x, shellRow!.marker.y)
    expect(opened).toEqual([])
  })

  test("follows growing live output only from the bottom", async () => {
    const setup = await createTestRenderer({ width: 70, height: 10, useThread: false })
    renderer = setup.renderer
    const workspace = new ToolsWorkspaceRenderable(renderer, kennelTheme, {
      onOpenToolOutput() { },
    })
    renderer.root.add(workspace)
    workspace.resizeForTerminal(70, 10)
    const initial = Array.from({ length: 6 }, (_, index) =>
      toolsActivity(`scroll-${index}`, 1, "running", 1))
    workspace.update(toolsWorkspaceModel(initial))
    await setup.renderOnce()

    workspace.activityScroller.scrollTo(workspace.activityScroller.scrollHeight)
    await setup.renderOnce()
    const bottomBeforeGrowth = workspace.activityScroller.scrollTop
    workspace.update(toolsWorkspaceModel(initial.map((row, index) =>
      index === 5 ? toolsActivity(row.invocationId, 8, "running", 1) : row)))
    await setup.renderOnce()
    expect(workspace.activityScroller.scrollTop).toBeGreaterThanOrEqual(bottomBeforeGrowth)

    workspace.activityScroller.scrollTo(0)
    await setup.renderOnce()
    workspace.update(toolsWorkspaceModel(initial.map((row, index) =>
      index === 4 ? toolsActivity(row.invocationId, 8, "running", 1) : row)))
    await setup.renderOnce()
    expect(workspace.activityScroller.scrollTop).toBe(0)
  })

  test("uses exact 74 divider 35 rail geometry and removes the rail below 100 columns", async () => {
    const setup = await createTestRenderer({ width: 110, height: 27, useThread: false })
    renderer = setup.renderer
    const workspace = new ToolsWorkspaceRenderable(renderer, kennelTheme, {
      onOpenToolOutput() { },
    })
    renderer.root.add(workspace)
    workspace.update(toolsWorkspaceModel([toolsActivity("geometry", 1, "running", 1)]))
    workspace.resizeForTerminal(110, 27)
    await setup.renderOnce()

    expect(workspace.activityPane.x).toBe(0)
    expect(workspace.activityPane.width).toBe(74)
    expect(workspace.turnRail.x).toBe(74)
    expect(workspace.turnRail.width).toBe(36)
    expect(workspace.turnSummary.x).toBe(75)
    expect(workspace.header.plainText).toBe("● rottweiler  running tools")

    workspace.resizeForTerminal(99, 27)
    await setup.renderOnce()
    expect(workspace.turnRail.visible).toBeFalse()
    expect(workspace.activityPane.width).toBe(99)
  })
})
