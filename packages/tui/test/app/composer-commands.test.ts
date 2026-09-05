import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, CommandOutcome, EngineEvent } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptyHistoryReader, historyReaderFor, waitForHistory, commandItem } from "../fixtures/history"

describe("Rottweiler composer-commands", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("opens slash autocomplete and gives the shared picker complete wrapped navigation", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const commands = Array.from({ length: 15 }, (_, index) => ({
      name: `command-${index}`,
      description: `Command ${index}`,
      usage: `/command-${index}`,
    }))
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: { ...createInitialState(), commands },
      onCommand: () => ({ type: "accepted" }),
    })
    renderer.root.add(app)
    await setup.mockInput.typeText("/")
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    await setup.renderOnce()
    const commandSpans = setup.captureSpans().lines.flatMap((line) => line.spans)
    const selectedTitle = commandSpans.find((span) => span.text.includes("/new"))
    const selectedCaption = commandSpans.find((span) => span.text.includes("Start a new conversation"))
    const nextCommand = commandSpans.find((span) => span.text.includes("/models"))
    expect(selectedTitle).toBeDefined()
    expect(selectedCaption).toBeDefined()
    expect(nextCommand).toBeDefined()
    expect(selectedTitle?.fg.toInts()).toEqual(selectedCaption?.fg.toInts())
    expect(selectedCaption?.fg.toInts()).not.toEqual(nextCommand?.fg.toInts())
    const optionCount = app.picker.select.options.length

    setup.mockInput.pressKey("p", { ctrl: true })
    expect(app.picker.select.getSelectedIndex()).toBe(optionCount - 1)
    setup.mockInput.pressKey("n", { ctrl: true })
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    setup.mockInput.pressKey("\x1b[6~")
    expect(app.picker.select.getSelectedIndex()).toBe(10)
    setup.mockInput.pressKey("\x1b[5~")
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    setup.mockInput.pressKey("END")
    expect(app.picker.select.getSelectedIndex()).toBe(optionCount - 1)
    setup.mockInput.pressKey("HOME")
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    setup.mockInput.pressArrow("up")
    expect(app.picker.select.getSelectedIndex()).toBe(optionCount - 1)
    setup.mockInput.pressArrow("down")
    expect(app.picker.select.getSelectedIndex()).toBe(0)

    const engineCommandIndex = app.picker.select.options.findIndex(
      (option) => option.value === "command-0",
    )
    expect(engineCommandIndex).toBeGreaterThanOrEqual(0)
    app.picker.select.setSelectedIndex(engineCommandIndex)
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(app.picker.visible).toBeFalse()
    expect(app.composer.value).toBe("")
  })

  test("positions the first slash palette above the composer and keeps that layout on reopen", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { historyReader: emptyHistoryReader })
    renderer.root.add(app)

    // Exercise the real first-input path before OpenTUI has completed a prior frame.
    await setup.mockInput.typeText("/")
    const firstConfiguredTop = app.picker.top
    expect(firstConfiguredTop).toBeGreaterThanOrEqual(0)
    await setup.renderOnce()
    const first = { y: app.picker.y, height: app.picker.height }
    expect(first.y + first.height).toBeLessThanOrEqual(app.composer.y)

    app.closePicker()
    app.composer.value = ""
    await setup.mockInput.typeText("/")
    await setup.renderOnce()
    expect({ y: app.picker.y, height: app.picker.height }).toEqual(first)
    expect(app.picker.y + app.picker.height).toBeLessThanOrEqual(app.composer.y)
  })

  test("keeps the composer pasteable while recovery rejects a submit and accepts its retry", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    let resolveRecovery!: (outcome: CommandOutcome) => void
    const recovery = new Promise<CommandOutcome>((resolve) => {
      resolveRecovery = resolve
    })
    let attempts = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) {
        if (command.type !== "send_message") return { type: "accepted" }
        attempts += 1
        return attempts === 1 ? recovery : { type: "accepted" }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    await setup.mockInput.pasteBracketedText("draft before recovery")
    setup.mockInput.pressEnter()
    await Promise.resolve()
    await setup.mockInput.pasteBracketedText(" and during recovery")
    expect(app.composer.value).toBe(" and during recovery")

    resolveRecovery({
      type: "rejected",
      error: {
        category: "protocol",
        code: "session_requires_recovery",
        message: "session is fail-closed until checkpoint journal recovery completes",
        retryable: true,
      },
    })
    await Bun.sleep(0)
    expect(app.composer.value).toBe("draft before recovery\n and during recovery")
    expect(app.state.errors.at(-1)?.code).toBe("session_requires_recovery")

    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(attempts).toBe(2)
    expect(app.composer.value).toBe("")
  })

  test("moves anchored slash selection to the closest match as the query changes", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        commands: [
          { name: "help", description: "List available commands", usage: "/help" },
          { name: "status", description: "Show session status", usage: "/status" },
        ],
      },
    })
    renderer.root.add(app)

    await setup.mockInput.typeText("/")
    expect(app.picker.select.getSelectedOption()?.value).toBe("new")
    await setup.mockInput.typeText("sta")
    expect(app.picker.select.getSelectedOption()?.value).toBe("status")
    app.closePicker()
    app.composer.value = ""
    await setup.mockInput.typeText("/pro")
    expect(app.picker.select.getSelectedOption()?.value).toBe("providers")
  })

  test("exposes /theme and opens the live theme picker from slash autocomplete", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { historyReader: emptyHistoryReader })
    renderer.root.add(app)

    await setup.mockInput.typeText("/the")
    expect(app.picker.select.getSelectedOption()?.value).toBe("theme")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)

    expect(app.themeBrowser.visible).toBeTrue()
    expect(app.themeBrowser.heading.plainText).toContain("THEME")
    expect(app.themeBrowser.itemIds.length).toBeGreaterThan(20)
    expect(app.themeBrowser.itemIds).toContain("theme:opencode")
  })

  test("executes a selected no-argument slash command on Enter and renders its result", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const items: import("../../src/protocol").TranscriptItem[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: historyReaderFor(items),
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        commands: [{ name: "status", description: "Show session status", usage: "/status" }],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    await setup.mockInput.typeText("/sta")
    expect(app.picker.select.getSelectedOption()?.value).toBe("status")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)

    expect(emitted).toContainEqual(expect.objectContaining({
      type: "send_message",
      content: "/status",
    }))
    expect(app.picker.visible).toBeFalse()
    expect(app.composer.value).toBe("")

    items.push(commandItem(1, "status", "actor idle · queue empty"))
    app.handleEvent({
      type: "command_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      name: "status",
      message: "actor idle · queue empty",
      unrestorable_paths: [],
    })
    await waitForHistory(setup, () => app.transcript.mountedCards.has("1"))
    const commandCard = [...app.transcript.mountedCards.values()].at(-1)
    expect(commandCard?.header.plainText).toBe("/status")
    expect(commandCard?.markdown.content).toContain("actor idle · queue empty")
  })

  test("answers free-text questions through one contained composer-backed dock", async () => {
    const setup = await createTestRenderer({ width: 80, height: 10, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.handleEvent({
      type: "question_asked",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      turn_id: "1",
      question_id: "question-text",
      questions: [{
        id: "question-text",
        prompt: "What should change?",
        response_kind: "text",
        options: [],
      }],
    })

    await setup.renderOnce()

    expect(app.interactionPanel.select.visible).toBeFalse()
    expect(app.interactionPanel.usesComposer).toBeTrue()
    expect(app.composer.visible).toBeTrue()
    expect(app.interactionPanel.prompt.plainText).toContain("Type your answer below")
    expect(app.interactionPanel.y + app.interactionPanel.height).toBeLessThanOrEqual(app.composer.y)
    expect(app.composer.y + app.composer.height).toBeLessThanOrEqual(app.statusLine.y)
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
    app.composer.value = Array.from({ length: 12 }, (_, index) => `answer-${index}`).join("\n")
    await setup.renderOnce()
    expect(app.interactionPanel.y + app.interactionPanel.height).toBeLessThanOrEqual(app.composer.y)
    expect(app.composer.y + app.composer.height).toBeLessThanOrEqual(app.statusLine.y)
    app.composer.value = ""
    const exact = "  first line\nsecond line  "
    await setup.mockInput.pasteBracketedText(exact)
    expect(app.composer.value).toBe(exact)
    expect(await app.composer.submit()).toBeTrue()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "answer_question",
      question_id: "question-text",
      answers: [{ question_id: "question-text", values: [exact] }],
    }))
  })

  test("omits unavailable telemetry and clears a friendly recovery banner on success", async () => {
    const setup = await createTestRenderer({ width: 100, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "disconnected", attempt: 7, error: null, gap: null },
      },
    })
    renderer.root.add(app)

    expect(app.banner.plainText).toBe("Connection lost · retrying…")
    expect(app.banner.plainText).not.toContain("attempt")
    expect(app.banner.plainText).not.toContain("disconnected")
    expect(app.statusLine.plainText).toContain("EXECUTE")
    expect(app.statusLine.plainText).toContain("model not selected · Alt+M")
    expect(app.statusLine.plainText).not.toContain("ctx")
    expect(app.statusLine.plainText).not.toContain("cache")
    expect(app.statusLine.plainText).not.toContain("git")
    app.handleEvent({
      type: "error",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      error: {
        category: "internal",
        code: "session_requires_recovery",
        message: "session is fail-closed until checkpoint journal recovery completes",
        retryable: true,
      },
    })
    expect(app.banner.plainText).toBe("Restoring this session · input will be available shortly")
    expect(app.banner.plainText).not.toContain("fail-closed")
    expect(app.banner.plainText).not.toContain("checkpoint journal")

    app.handleEvent({
      type: "turn_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      turn_id: "1",
    })
    expect(app.banner.plainText).toBe("Connection lost · retrying…")
    expect(app.banner.plainText).not.toContain("recovery")
    expect(app.state.errors).toHaveLength(0)
  })

  test("lists only /exit and closes the supervised app without sending protocol text", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let exits = 0
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
      onExit() {
        exits += 1
      },
    })
    renderer.root.add(app)

    await setup.mockInput.typeText("/ex")
    expect(app.picker.select.getSelectedOption()?.value).toBe("exit")
    expect(app.picker.select.options.some((option) => option.value === "quit")).toBeFalse()
    emitted.length = 0
    setup.mockInput.pressEnter()
    await Bun.sleep(0)

    expect(exits).toBe(1)
    expect(emitted).toEqual([])
    expect(app.composer.value).toBe("")

    app.composer.value = "/quit"
    expect(await app.composer.submit()).toBeTrue()
    expect(exits).toBe(1)
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "send_message",
      content: "/quit",
    }))
  })
})
