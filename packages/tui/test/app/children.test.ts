import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, CommandOutcome, EngineEvent } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptyHistoryReader } from "../fixtures/history"

describe("Rottweiler children", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("opens the child-agent tree from the global Ctrl+G binding", async () => {
    const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "parent-session",
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    setup.mockInput.pressKey("g", { ctrl: true })
    await Bun.sleep(0)
    expect(app.picker.title).toContain("Child agents")
    expect(emitted.at(-1)).toMatchObject({
      type: "list_subagents",
      session_id: "parent-session",
    })
  })

  test("uses Escape to return to the parent and double Escape to interrupt a running child", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "parent draft stays private"
    app.composer.addAttachment({
      name: "parent context.txt",
      media_type: "text/plain",
      data: { type: "text", content: "parent only" },
    })
    app.openSubagentPicker()
    const list = emitted.find((command) => command.type === "list_subagents")!
    app.handleEvent({
      type: "subagents_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: list.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "parent-session",
      subagents: [{
        subagent_id: "child-running",
        child_session_id: "child-session",
        task: "Review runtime",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.activeSubagentId).toBe("child-running")
    expect(app.composer.value).toBe("")
    expect(app.composer.attachments).toEqual([])
    app.composer.value = "child-only follow-up"

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.activeSubagentId).toBeNull()
    expect(app.composer.value).toBe("parent draft stays private")
    expect(app.composer.attachments.map((attachment) => attachment.name)).toEqual([
      "parent context.txt",
    ])
    expect(app.banner.plainText).toContain("press Esc again to stop the child agent")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(emitted.at(-1)).toMatchObject({
      type: "interrupt_subagent",
      session_id: "parent-session",
      subagent_id: "child-running",
    })
  })

  test("leaves Vim insert mode before Escape exits a child transcript", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "parent-session",
      keybindings: { preset: "vim" },
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openSubagentPicker()
    const list = emitted.find((command) => command.type === "list_subagents")!
    app.handleEvent({
      type: "subagents_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: list.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "parent-session",
      subagents: [{
        subagent_id: "child-vim",
        child_session_id: "child-session",
        task: "Vim child",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.activeSubagentId).toBe("child-vim")
    setup.mockInput.pressKey("i")
    expect(app.composer.hintText.plainText).toContain("INSERT")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.activeSubagentId).toBe("child-vim")
    expect(app.composer.hintText.plainText).toContain("NORMAL")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.activeSubagentId).toBeNull()
  })

  test("shows running child state without offering or selecting a follow-up action", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openSubagentPicker()
    const list = emitted.find((command) => command.type === "list_subagents")!
    app.handleEvent({
      type: "subagents_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: list.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "parent-session",
      subagents: [{
        subagent_id: "child-running-actions",
        child_session_id: "child-session",
        task: "Finish current work",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.closePicker()
    app.openSubagentActionPicker("child-running-actions")
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "inspect",
      "running",
      "interrupt",
      "close",
    ])
    expect(app.picker.select.options.map((option) => option.name).join(" ")).not.toContain(
      "Send follow-up",
    )
    app.picker.moveSelection(1)
    expect(app.picker.select.getSelectedOption()?.value).toBe("interrupt")

    const commandCount = emitted.length
    app.picker.select.setSelectedIndex(1)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.picker.visible).toBeTrue()
    expect(emitted).toHaveLength(commandCount)
    expect(emitted.some((command) => command.type === "continue_subagent")).toBeFalse()
  })

  test("keeps child-list failures retryable instead of claiming the list is empty", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    let attempts = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "parent-session",
      onCommand(command) {
        if (command.type === "list_subagents") {
          attempts += 1
          return {
            type: "rejected",
            error: {
              category: "protocol",
              code: "offline",
              message: "engine temporarily unavailable",
              retryable: true,
            },
          }
        }
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openSubagentPicker()
    await Bun.sleep(0)
    expect(app.picker.select.options.map((option) => option.value)).toEqual(["agents.retry"])
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(attempts).toBe(2)
  })

  test("restores a rejected child submission only to its originating child draft", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let rejectFollowUp: ((outcome: CommandOutcome) => void) | undefined
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        if (command.type === "continue_subagent") {
          return new Promise<CommandOutcome>((resolve) => {
            rejectFollowUp = resolve
          })
        }
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "parent draft"
    app.openSubagentPicker()
    const list = emitted.find((command) => command.type === "list_subagents")!
    app.handleEvent({
      type: "subagents_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: list.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "parent-session",
      subagents: [{
        subagent_id: "child-draft",
        child_session_id: "child-session",
        task: "Keep drafts isolated",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "idle",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    app.composer.value = "child submission that will fail"
    const submission = app.composer.submit()
    await Bun.sleep(0)
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.activeSubagentId).toBeNull()
    expect(app.composer.value).toBe("parent draft")

    rejectFollowUp?.({
      type: "rejected",
      error: {
        category: "protocol",
        code: "child_busy",
        message: "child is temporarily busy",
        retryable: true,
      },
    })
    expect(await submission).toBeFalse()
    expect(app.composer.value).toBe("parent draft")
    app.openSubagentActionPicker("child-draft")
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.activeSubagentId).toBe("child-draft")
    expect(app.composer.value).toBe("child submission that will fail")
  })

  test("keeps the newly inspected child active when an older child shell command is accepted", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let acceptShell: ((outcome: CommandOutcome) => void) | undefined
    let request = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      terminalHandover: { suspend() { }, resume() { } },
      onCommand(command) {
        emitted.push(command)
        if (command.type === "user_shell_started") {
          return new Promise<CommandOutcome>((resolve) => {
            acceptShell = resolve
          })
        }
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openSubagentPicker()
    const list = emitted.find((command) => command.type === "list_subagents")!
    app.handleEvent({
      type: "subagents_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: list.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "parent-session",
      subagents: [
        {
          subagent_id: "child-a",
          child_session_id: "child-session-a",
          task: "Origin child",
          agent: "reviewer",
          model: "fast",
          isolation: "shared",
          activity: "running",
        },
        {
          subagent_id: "child-b",
          child_session_id: "child-session-b",
          task: "New child",
          agent: "reviewer",
          model: "fast",
          isolation: "shared",
          activity: "running",
        },
      ],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.activeSubagentId).toBe("child-a")
    app.composer.value = "!pwd"
    const submission = app.composer.submit()
    await Bun.sleep(0)
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    app.openSubagentActionPicker("child-b")
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.activeSubagentId).toBe("child-b")
    acceptShell?.({ type: "accepted" })
    expect(await submission).toBeTrue()
    expect(app.activeSubagentId).toBe("child-b")
  })
})
