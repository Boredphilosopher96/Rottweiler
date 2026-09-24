import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, CommandOutcome } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader, sessionReaderFor, waitForHistory, commandItem } from "../fixtures/history"

describe("Rottweiler composer-commands", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("slash and Ctrl+P open the same command palette without consuming attachments", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)
    app.composer.addAttachment({ name: "notes.txt", media_type: "text/plain", data: { type: "text", content: "Keep me" } })
    await setup.mockInput.typeText("/")
    expect(app.commandPalette.visible).toBeTrue()
    expect(app.composer.value).toBe("")
    expect(app.composer.attachments).toHaveLength(1)
    const slashIds = app.commandPalette.itemIds
    app.closePicker()
    setup.mockInput.pressKey("p", { ctrl: true })
    expect(app.commandPalette.itemIds).toEqual(slashIds)
    expect(app.composer.attachments).toHaveLength(1)
    app.closePicker()
    await setup.mockInput.pasteBracketedText("/compact preserve the API")
    expect(app.commandPalette.visible).toBeFalse()
    expect(app.composer.value).toBe("/compact preserve the API")
  })

  test("positions the shared slash palette across the primary surface on reopen", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)

    // Exercise the real first-input path before OpenTUI has completed a prior frame.
    await setup.mockInput.typeText("/")
    const firstConfiguredTop = app.commandPalette.top
    expect(firstConfiguredTop).toBeGreaterThanOrEqual(0)
    await setup.renderOnce()
    const first = { y: app.commandPalette.y, height: app.commandPalette.height }
    expect(first.y + first.height).toBeLessThanOrEqual(app.composer.y)

    app.closePicker()
    app.composer.value = ""
    await setup.mockInput.typeText("/")
    await setup.renderOnce()
    expect({ y: app.commandPalette.y, height: app.commandPalette.height }).toEqual(first)
    expect(app.commandPalette.y + app.commandPalette.height).toBeLessThanOrEqual(app.composer.y)
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
      sessionReader: emptySessionReader,
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

  test("filters the shared slash palette as the query changes", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
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
    expect(app.commandPalette.visible).toBeTrue()
    await setup.mockInput.typeText("sta")
    expect(app.commandPalette.selectedId).toBe("status.show")
    app.closePicker()
    app.composer.value = ""
    await setup.mockInput.typeText("/")
    await setup.mockInput.typeText("pro")
    expect(app.commandPalette.itemIds).toContain("provider.list")
  })

  test("exposes /theme and opens the live theme picker from slash autocomplete", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)

    await setup.mockInput.typeText("/")
    await setup.mockInput.typeText("theme")
    expect(app.commandPalette.selectedId).toBe("theme.list")
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
      sessionReader: sessionReaderFor(items),
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

    await setup.mockInput.typeText("/")
    await setup.mockInput.typeText("sta")
    expect(app.commandPalette.selectedId).toBe("status.show")
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
      sessionReader: emptySessionReader,
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
      question: {
        id: "question-text",
        prompt: "What should change?",
        response_kind: "text",
        options: [],
      },
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
      answer: { question_id: "question-text", value: exact },
    }))
  })

  test("omits unavailable telemetry and clears a friendly recovery banner on success", async () => {
    const setup = await createTestRenderer({ width: 100, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
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
      sessionReader: emptySessionReader,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
      onExit() {
        exits += 1
      },
    })
    renderer.root.add(app)

    await setup.mockInput.typeText("/")
    await setup.mockInput.typeText("exit")
    expect(app.commandPalette.selectedId).toBe("app.exit")
    expect(app.commandPalette.itemIds).not.toContain("slash.quit")
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
