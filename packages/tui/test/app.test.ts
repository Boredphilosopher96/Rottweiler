import { afterEach, describe, expect, test } from "bun:test"
import { CliRenderEvents } from "@opentui/core"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"

import { createRottweilerApp, type PresentationFrameScheduler } from "../src/app"
import { colorContrast, pickerSelectionColors } from "../src/components/picker"
import type { ClientCommand, CommandOutcome, EngineEvent } from "../src/protocol"
import { PROTOCOL_VERSION } from "../../../protocol/types"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
import {
  daylightTheme,
  kennelTheme,
  systemThemeFor,
  themeByName,
  themeCatalog,
  themeCatalogFor,
  type RottweilerTheme,
} from "../src/theme"

const initialEvent = {
  type: "text_delta",
  meta: {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-tui-test",
    sequence_id: "1",
    emitted_at: "2026-01-01T00:00:00Z",
  },
  turn_id: "turn-tui-test",
  text: "hello",
} satisfies EngineEvent

class ManualPresentationFrame implements PresentationFrameScheduler {
  #next = 0
  readonly callbacks = new Map<number, () => void>()
  readonly delays: number[] = []
  scheduled = 0

  schedule(callback: () => void, delayMs: number): number {
    const handle = ++this.#next
    this.callbacks.set(handle, callback)
    this.delays.push(delayMs)
    this.scheduled += 1
    return handle
  }

  cancel(handle: unknown): void {
    if (typeof handle === "number") this.callbacks.delete(handle)
  }

  flush(): void {
    const callbacks = [...this.callbacks.values()]
    this.callbacks.clear()
    for (const callback of callbacks) callback()
  }
}

function rgba(hex: string): [number, number, number, number] {
  return [
    Number.parseInt(hex.slice(1, 3), 16),
    Number.parseInt(hex.slice(3, 5), 16),
    Number.parseInt(hex.slice(5, 7), 16),
    255,
  ]
}

function expectCoherentTheme(app: ReturnType<typeof createRottweilerApp>, theme: RottweilerTheme) {
  expect(app.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.main.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.transcript.backgroundColor.toInts()).toEqual(rgba(theme.background))
  expect(app.contextPanel.backgroundColor.toInts()).toEqual(rgba(theme.panel))
  expect(app.composer.backgroundColor.toInts()).toEqual(rgba(theme.panel))
  expect(app.reviewPanel.backgroundColor.toInts()).toEqual(rgba(theme.panel))
  expect(app.interactionPanel.backgroundColor.toInts()).toEqual(rgba(theme.panelRaised))
  expect(app.picker.backgroundColor.toInts()).toEqual(rgba(theme.panelRaised))
  expect(app.statusLine.bg.toInts()).toEqual(rgba(theme.panel))
}

function completeTransportReconnect(app: ReturnType<typeof createRottweilerApp>): void {
  app.setState({
    ...app.state,
    connection: { phase: "reconnecting", attempt: 1, error: null, gap: null },
  })
  app.setState({
    ...app.state,
    connection: { phase: "connected", attempt: 1, error: null, gap: null },
  })
}

describe("Rottweiler OpenTUI shell", () => {
  let renderer: TestRenderer | undefined

  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("refreshes live runtime services around tool execution", async () => {
    const setup = await createTestRenderer({ width: 100, height: 18, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionId: "session-services",
      requestId: () => `request-${commands.length + 1}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.handleEvent({
      type: "tool_call_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-services",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      turn_id: "turn-services",
      tool_call_id: "tool-services",
      name: "edit",
      args: { path: "src/lib.rs" },
      call_index: 0,
    })
    expect(commands.at(-1)).toMatchObject({ type: "list_runtime_services" })

    app.handleEvent({
      type: "tool_call_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-services",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      turn_id: "turn-services",
      tool_call_id: "tool-services",
      output: { type: "text", text: "done" },
      is_error: false,
      call_index: 0,
    })
    expect(commands.filter((command) => command.type === "list_runtime_services")).toHaveLength(2)
  })

  test("clears stale runtime services when the final activity refresh fails", async () => {
    const setup = await createTestRenderer({ width: 100, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionId: "session-services",
      initialState: {
        ...createInitialState(),
        runtimeServices: [{ kind: "formatter", name: "rustfmt" }],
      },
      onCommand(command) {
        if (command.type !== "list_runtime_services") return { type: "accepted" }
        return {
          type: "rejected",
          error: {
            category: "protocol",
            code: "services_unavailable",
            message: "service probe failed",
            retryable: true,
          },
        }
      },
    })
    renderer.root.add(app)

    app.handleEvent({
      type: "tool_call_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-services",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      turn_id: "turn-services",
      tool_call_id: "tool-services",
      name: "edit",
      args: { path: "src/lib.rs" },
      call_index: 0,
    })
    app.handleEvent({
      type: "tool_call_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-services",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      turn_id: "turn-services",
      tool_call_id: "tool-services",
      output: { type: "text", text: "done" },
      is_error: false,
      call_index: 0,
    })
    await Bun.sleep(0)

    expect(app.state.runtimeServices).toEqual([])
    expect(app.state.errors.at(-1)?.message).toContain("service probe failed")
  })

  test("renders into OpenTUI's inspectable in-memory cell buffer", async () => {
    const setup = await createTestRenderer({
      width: 72,
      height: 12,
      useThread: false,
    })
    renderer = setup.renderer
    renderer.root.add(createRottweilerApp(renderer, { initialEvent }))

    await setup.renderOnce()

    const frame = setup.captureCharFrame()
    expect(frame).toContain("Rottweiler")
    expect(frame).toContain("hello")
    expect(frame).toContain("model —")

    const cells = setup.captureSpans()
    expect(cells.cols).toBe(72)
    expect(cells.rows).toBe(12)
    expect(cells.lines).toHaveLength(12)
  })

  test("coalesces hundreds of ordered presentation deltas into one frame without losing protocol progress", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const frame = new ManualPresentationFrame()
    const app = createRottweilerApp(renderer, { presentationFrame: frame })
    renderer.root.add(app)
    app.handleEvent({
      type: "turn_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      turn_id: "turn-stream",
    })
    let presentationUpdates = 0
    const update = app.transcript.update.bind(app.transcript)
    app.transcript.update = (state) => {
      presentationUpdates += 1
      update(state)
    }

    for (let index = 0; index < 300; index += 1) {
      const sequence = String(index + 2)
      const meta = {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: sequence,
        emitted_at: "2026-01-01T00:00:00Z",
      } as const
      if (index % 3 === 0) {
        app.handleEvent({ type: "text_delta", meta, turn_id: "turn-stream", text: "t" })
      } else if (index % 3 === 1) {
        app.handleEvent({ type: "thinking_delta", meta, turn_id: "turn-stream", text: "r" })
      } else {
        app.handleEvent({
          type: "citation_delta",
          meta,
          turn_id: "turn-stream",
          uri: `https://example.test/${index}`,
        })
      }
    }

    expect(frame.scheduled).toBe(1)
    expect(frame.delays).toEqual([16])
    expect(frame.callbacks.size).toBe(1)
    expect(presentationUpdates).toBe(0)
    expect(app.state.lastSequence).toBe("301")
    expect(app.state.streamingTail?.text).toHaveLength(100)
    expect(app.state.streamingTail?.thinking).toHaveLength(100)
    expect(app.state.streamingTail?.citations).toHaveLength(100)

    frame.flush()

    expect(presentationUpdates).toBe(1)
    expect(frame.callbacks.size).toBe(0)
    expect(app.transcript.streamingMarkdown.content).toHaveLength(100)
  })

  test("coalesces compaction text and thinking into one presentation frame", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const frame = new ManualPresentationFrame()
    const app = createRottweilerApp(renderer, { presentationFrame: frame })
    renderer.root.add(app)
    app.handleEvent({
      type: "compaction_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      reason: "automatic",
    })
    app.handleEvent({
      type: "compaction_attempt_started",
      session_id: "session-local",
      summary_turn_id: "7",
      attempt: 0,
    })

    for (let index = 0; index < 200; index += 1) {
      app.handleEvent({
        type: index % 2 === 0 ? "compaction_text_delta" : "compaction_thinking_delta",
        session_id: "session-local",
        summary_turn_id: "7",
        attempt: 0,
        text: "x",
      })
    }

    expect(frame.scheduled).toBe(1)
    expect(frame.callbacks.size).toBe(1)
    frame.flush()
    expect(app.state.compaction?.text).toHaveLength(100)
    expect(app.state.compaction?.thinking).toHaveLength(100)
  })

  test("flushes queued stream content immediately before permission, question, and finish events", async () => {
    const terminalEvents: EngineEvent[] = [
      {
        type: "tool_approval_needed",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "3",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
        tool_call_id: "tool-1",
        name: "bash",
        args: { command: "pwd" },
        capabilities: ["execute"],
        rationale: "inspect the workspace",
      },
      {
        type: "question_asked",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "3",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
        question_id: "question-1",
        questions: [{
          id: "question-1",
          prompt: "Continue?",
          response_kind: "select_one",
          options: [{ value: "yes", label: "Yes" }],
        }],
      },
      {
        type: "turn_finished",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "3",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
        status: "completed",
        usage: {
          input_tokens: "1",
          output_tokens: "1",
          cache_read_tokens: "0",
          cache_write_tokens: "0",
          reasoning_tokens: "0",
        },
        cost: { kind: "unavailable", reason: "fixture" },
      },
    ]

    for (const terminalEvent of terminalEvents) {
      const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
      renderer = setup.renderer
      const frame = new ManualPresentationFrame()
      const app = createRottweilerApp(renderer, { presentationFrame: frame, onCommand: () => ({ type: "accepted" }) })
      renderer.root.add(app)
      app.handleEvent({
        type: "turn_started",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "1",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
      })
      let presentationUpdates = 0
      const update = app.transcript.update.bind(app.transcript)
      app.transcript.update = (state) => {
        presentationUpdates += 1
        update(state)
      }
      app.handleEvent({
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "2",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
        text: "ready",
      })

      app.handleEvent(terminalEvent)

      expect(presentationUpdates).toBe(1)
      expect(frame.callbacks.size).toBe(0)
      expect(app.state.lastSequence).toBe("3")
      expect(app.transcript.streamingMarkdown.content).toBe("ready")
      if (terminalEvent.type === "tool_approval_needed") expect(app.interactionPanel.visible).toBeTrue()
      if (terminalEvent.type === "question_asked") expect(app.interactionPanel.visible).toBeTrue()
      if (terminalEvent.type === "turn_finished") expect(app.state.turns["turn-terminal"]?.status).toBe("completed")
      renderer.destroy()
      renderer = undefined
    }
  })

  test("constructs the complete app with the persisted startup theme", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    renderer.root.add(createRottweilerApp(renderer, { theme: daylightTheme }))

    await setup.renderOnce()

    const backgrounds = setup.captureSpans().lines.flatMap((line) =>
      line.spans.map((span) => span.bg.toInts())
    )
    expect(backgrounds).toContainEqual(rgba(daylightTheme.background))
    expect(backgrounds).not.toContainEqual(rgba(kennelTheme.background))
  })

  test("previews the dynamic theme catalog coherently, reverts on Escape, and persists on confirm", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      theme: kennelTheme,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "draft survives retheme"

    app.openThemePicker()
    expect(app.picker.select.options.map((option) => option.value)).toEqual(
      themeCatalog.map((theme) => `theme:${theme.name}`),
    )
    const previewTheme = themeByName("tokyonight")!
    const previewIndex = app.picker.select.options.findIndex(
      (option) => option.value === `theme:${previewTheme.name}`,
    )
    const pickerBeforePreview = app.picker
    app.picker.select.setSelectedIndex(previewIndex)
    await setup.renderOnce()
    // Theme preview rebuilds the themed render tree while preserving picker
    // query, selection, focus, and the composer draft.
    expect(app.picker === pickerBeforePreview).toBeFalse()
    expect(pickerBeforePreview.input.isDestroyed).toBeTrue()
    expect(app.picker.input.isDestroyed).toBeFalse()
    expect(renderer.currentFocusedRenderable?.id).toBe("picker-query")
    expect(setup.captureCharFrame()).toContain("Themes · arrows preview · Enter confirms")
    expectCoherentTheme(app, previewTheme)
    expect(app.composer.value).toBe("draft survives retheme")

    setup.mockInput.pressEscape()
    await Bun.sleep(100)
    await setup.renderOnce()
    expect(app.picker.visible).toBeFalse()
    expect(renderer.currentFocusedRenderable?.id).toBe("composer-editor")
    expectCoherentTheme(app, kennelTheme)
    expect(app.composer.value).toBe("draft survives retheme")
    expect(commands).toHaveLength(0)

    app.openThemePicker()
    app.picker.select.setSelectedIndex(
      app.picker.select.options.findIndex((option) => option.value === `theme:${previewTheme.name}`),
    )
    app.picker.select.selectCurrent()
    await Bun.sleep(10)
    expect(commands).toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "ui.theme",
      value: previewTheme.name,
    }))
    expect(app.picker.visible).toBeFalse()
    expectCoherentTheme(app, previewTheme)

    setup.resize(64, 14)
    app.openModePicker()
    await setup.renderOnce()
    expect(app.picker.visible).toBeTrue()
    expectCoherentTheme(app, previewTheme)
    expect(setup.captureCharFrame()).toContain("Modes")
  })

  test("keeps the active System theme and its picker preview synchronized with terminal mode", async () => {
    const setup = await createTestRenderer({ width: 90, height: 22, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      theme: systemThemeFor("dark"),
      systemThemeMode: "dark",
    })
    renderer.root.add(app)
    await setup.renderOnce()
    expectCoherentTheme(app, systemThemeFor("dark"))

    renderer.emit(CliRenderEvents.THEME_MODE, "light")
    await setup.renderOnce()
    expectCoherentTheme(app, systemThemeFor("light"))

    app.openThemePicker()
    const system = app.picker.select.options.find((option) => option.value === "theme:system")
    expect(system?.description).toContain(daylightTheme.background)
  })

  test("previews every built-in theme in the terminal's current light variant", async () => {
    const setup = await createTestRenderer({ width: 90, height: 22, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      theme: daylightTheme,
      systemThemeMode: "light",
    })
    renderer.root.add(app)
    app.openThemePicker()
    const preview = themeByName("tokyonight", "light")!
    app.picker.select.setSelectedIndex(
      app.picker.select.options.findIndex((option) => option.value === "theme:tokyonight"),
    )
    await setup.renderOnce()
    expectCoherentTheme(app, preview)
  })

  test("submits with plain Enter while modified Enter and Ctrl+J insert newlines", async () => {
    const setup = await createTestRenderer({
      width: 72,
      height: 12,
      useThread: false,
      kittyKeyboard: true,
    })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    await setup.mockInput.typeText("first")
    setup.mockInput.pressEnter({ shift: true })
    await setup.mockInput.typeText("second")
    setup.mockInput.pressEnter({ ctrl: true })
    await setup.mockInput.typeText("third")
    setup.mockInput.pressEnter({ meta: true })
    await setup.mockInput.typeText("fourth")
    setup.mockInput.pressKey("j", { ctrl: true })
    await setup.mockInput.typeText("fifth")
    expect(app.composer.value).toBe("first\nsecond\nthird\nfourth\nfifth")
    expect(commands).toHaveLength(0)

    setup.mockInput.pressEnter()
    await Bun.sleep(10)
    expect(commands).toContainEqual(
      expect.objectContaining({
        type: "send_message",
        content: "first\nsecond\nthird\nfourth\nfifth",
      }),
    )
    expect(app.composer.value).toBe("")
  })

  test("keeps a long composer draft bounded and internally scrolled at 45x10", async () => {
    const setup = await createTestRenderer({ width: 45, height: 10, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)
    app.composer.value = Array.from({ length: 20 }, (_, index) => `draft-${index}`).join("\n")
    app.composer.editor.gotoBufferEnd()
    await setup.renderOnce()

    expect(app.composer.y).toBeGreaterThanOrEqual(0)
    expect(app.composer.y + app.composer.height).toBeLessThanOrEqual(10)
    expect(app.composer.editor.y).toBeGreaterThan(app.composer.y)
    expect(app.composer.editor.y + app.composer.editor.height).toBeLessThan(
      app.composer.y + app.composer.height,
    )
    expect(app.composer.editor.scrollY).toBeGreaterThan(0)
    expect(setup.captureSpans().lines).toHaveLength(10)
  })

  test("grows the composer for one visually wrapped logical line on a narrow terminal", async () => {
    const setup = await createTestRenderer({ width: 20, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)
    app.composer.value = "wrapped-draft ".repeat(12).trim()
    await setup.renderOnce()

    expect(app.composer.editor.lineCount).toBe(1)
    expect(app.composer.height).toBe(7)
    expect(app.composer.y + app.composer.height).toBeLessThanOrEqual(18)
  })

  test("contains the transcript, composer, and status at short and normal terminal heights", async () => {
    for (const height of [8, 12, 18]) {
      const setup = await createTestRenderer({ width: 45, height, useThread: false })
      renderer = setup.renderer
      const app = createRottweilerApp(renderer)
      renderer.root.add(app)
      app.composer.value = Array.from({ length: 24 }, (_, index) => `line-${index}`).join("\n")
      app.composer.editor.gotoBufferEnd()
      await setup.renderOnce()

      for (const component of [app.main, app.composer, app.statusLine]) {
        expect(component.y).toBeGreaterThanOrEqual(0)
        expect(component.y + component.height).toBeLessThanOrEqual(height)
      }
      expect(app.composer.editor.scrollY).toBeGreaterThan(0)
      expect(setup.captureSpans().lines).toHaveLength(height)
      renderer.destroy()
      renderer = undefined
    }
  })

  test("uses one constrained bottom-dock input for approvals, choices, and plans", async () => {
    const setup = await createTestRenderer({ width: 72, height: 10, useThread: false })
    renderer = setup.renderer
    const base = createInitialState()
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)

    const expectExclusiveInteraction = async () => {
      await setup.renderOnce()
      expect(app.interactionPanel.visible).toBeTrue()
      expect(app.interactionPanel.capturesInput).toBeTrue()
      expect(app.composer.visible).toBeFalse()
      expect(renderer?.currentFocusedRenderable).toBe(app.interactionPanel.select)
      expect(app.main.y + app.main.height).toBeLessThanOrEqual(app.interactionPanel.y)
      expect(app.interactionPanel.y + app.interactionPanel.height).toBeLessThanOrEqual(
        app.statusLine.y,
      )
      expect(app.interactionPanel.height).toBeLessThanOrEqual(8)
    }

    app.setState({
      ...base,
      tools: {
        edit: {
          toolCallId: "edit",
          turnId: "1",
          name: "edit",
          args: { path: "src/main.rs" },
          status: "awaiting_approval",
          capabilities: ["write_filesystem"],
          rationale: "Apply the reviewed change",
          diff: {
            proposal_id: "proposal",
            path: "src/main.rs",
            unified_diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n",
            arguments_hash: "arguments",
            base_hash: "base",
            diff_hash: "diff",
            truncated: false,
          },
          chunks: [],
          output: null,
          isError: null,
          callIndex: 0,
        },
      },
    })
    await expectExclusiveInteraction()
    expect(app.interactionPanel.select.height).toBeGreaterThan(0)

    app.setState({
      ...base,
      questions: {
        choice: {
          questionId: "choice",
          turnId: "2",
          questions: [{
            id: "choice",
            prompt: "Choose the safe option",
            response_kind: "select_one",
            options: [
              { value: "keep", label: "Keep", description: "Keep the change" },
              { value: "revert", label: "Revert", description: "Revert the change" },
            ],
          }],
          answers: null,
          answered: false,
        },
      },
    })
    await expectExclusiveInteraction()

    app.setState({
      ...base,
      mode: "plan",
      pendingPlan: {
        title: "Implement safely",
        summary_md: "One reviewed change.",
        steps: [{ description: "Edit", files_touched: ["src/lib.rs"], verification: "cargo test" }],
        open_questions: [],
      },
    })
    await expectExclusiveInteraction()

    app.setState(base)
    await setup.renderOnce()
    expect(app.interactionPanel.visible).toBeFalse()
    expect(app.composer.visible).toBeTrue()
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
    expect(app.composer.y + app.composer.height).toBeLessThanOrEqual(app.statusLine.y)
  })

  test("keeps anchored autocomplete above the composer on short terminals", async () => {
    for (const height of [8, 10, 12]) {
      const setup = await createTestRenderer({ width: 45, height, useThread: false })
      renderer = setup.renderer
      const app = createRottweilerApp(renderer, {
        initialState: {
          ...createInitialState(),
          commands: Array.from({ length: 20 }, (_, index) => ({
            name: `command-${index}`,
            description: `Command ${index}`,
            usage: `/command-${index}`,
          })),
        },
      })
      renderer.root.add(app)
      await setup.mockInput.typeText("/")
      await setup.renderOnce()

      expect(app.picker.y).toBeGreaterThanOrEqual(0)
      expect(app.picker.y + app.picker.height).toBeLessThanOrEqual(app.composer.y)
      renderer.destroy()
      renderer = undefined
    }
  })

  test("collapses image preview before it can hide the short-terminal editor", async () => {
    const setup = await createTestRenderer({ width: 45, height: 8, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)
    app.composer.addImage({ name: "screen.png", mediaType: "image/png", base64: "AA==" })
    app.composer.value = "visible draft"
    await setup.renderOnce()

    expect(app.composer.editor.visible).toBeTrue()
    expect(app.composer.editor.y).toBeGreaterThan(app.composer.y)
    expect(app.composer.editor.y + app.composer.editor.height).toBeLessThan(
      app.composer.y + app.composer.height,
    )
    expect(setup.captureCharFrame()).toContain("visible draft")
  })

  test("contains the changed-file diff overlay at short terminal heights", async () => {
    for (const height of [8, 10, 12]) {
      const setup = await createTestRenderer({ width: 112, height, useThread: false })
      renderer = setup.renderer
      const app = createRottweilerApp(renderer, {
        requestId: () => `short-diff-${height}`,
        onCommand: () => ({ type: "accepted" }),
        initialState: {
          ...createInitialState(),
          workspaceStatus: {
            workspaceName: "Rottweiler",
            branch: "main",
            changedPaths: ["src/exact.rs"],
            truncated: false,
          },
          review: {
            sessionId: "short-review",
            files: [{
              path: "src/exact.rs",
              unifiedDiff: "--- a/src/exact.rs\n+++ b/src/exact.rs\n-old\n+new\n",
              status: "pending",
              truncated: false,
              unrestorableReason: null,
              originalHash: "old",
              currentHash: "new",
            }],
          },
        },
      })
      renderer.root.add(app)
      await setup.renderOnce()
      app.contextPanel.changedFiles.selectCurrent()
      app.handleEvent({
        type: "workspace_diff_ready",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "tui-client",
          request_id: `short-diff-${height}`,
          emitted_at: "2026-01-01T00:00:00Z",
        },
        session_id: "session-local",
        diff: {
          path: "src/exact.rs",
          unified_diff: "--- a/src/exact.rs\n+++ b/src/exact.rs\n-old\n+new\n",
          truncated: false,
          binary: false,
        },
      })
      await setup.renderOnce()

      for (const component of [app.reviewPanel, app.statusLine]) {
        expect(component.y).toBeGreaterThanOrEqual(0)
        expect(component.y + component.height).toBeLessThanOrEqual(height)
      }
      expect(app.reviewPanel.diff.diff).toContain("+new")
      renderer.destroy()
      renderer = undefined
    }
  })

  test("keeps a workspace diff read-only and stable beside a retained session review", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      requestId: () => "stable-workspace-diff",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        workspaceStatus: {
          workspaceName: "Rottweiler",
          branch: "main",
          changedPaths: ["src/worktree.rs"],
          truncated: false,
        },
        review: {
          sessionId: "retained-review",
          files: [{
            path: "src/session-review.rs",
            unifiedDiff: "+session-review\n",
            status: "pending",
            truncated: false,
            unrestorableReason: null,
            originalHash: "old-session",
            currentHash: "new-session",
          }],
        },
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    app.contextPanel.changedFiles.selectCurrent()
    app.handleEvent({
      type: "workspace_diff_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "stable-workspace-diff",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      diff: {
        path: "src/worktree.rs",
        unified_diff: "+worktree-only\n",
        truncated: false,
        binary: false,
      },
    })
    setup.mockInput.pressKey("a")
    app.handleEvent({
      type: "text_delta",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      turn_id: "turn-after-diff",
      text: "unrelated event",
    })

    expect(commands.filter((command) => command.type === "review_file")).toHaveLength(0)
    expect(app.reviewPanel.title).toContain("Diff · src/worktree.rs")
    expect(app.reviewPanel.diff.diff).toContain("+worktree-only")
    expect(app.reviewPanel.diff.diff).not.toContain("session-review")
    expect(app.reviewPanel.hint.plainText).toBe("Esc close")
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
      initialState: { ...createInitialState(), commands },
      onCommand: () => ({ type: "accepted" }),
    })
    renderer.root.add(app)
    await setup.mockInput.typeText("/")
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    await setup.renderOnce()
    const commandSpans = setup.captureSpans().lines.flatMap((line) => line.spans)
    const selectedTitle = commandSpans.find((span) => span.text.includes("/help"))
    const selectedCaption = commandSpans.find((span) => span.text.includes("List available commands"))
    const nextCommand = commandSpans.find((span) => span.text.includes("/status"))
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

    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(app.picker.visible).toBeFalse()
    expect(app.composer.value).toBe("")
  })

  test("positions the first slash palette above the composer and keeps that layout on reopen", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
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
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)

    await setup.mockInput.typeText("/")
    expect(app.picker.select.getSelectedOption()?.value).toBe("help")
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
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)

    await setup.mockInput.typeText("/the")
    expect(app.picker.select.getSelectedOption()?.value).toBe("theme")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)

    expect(app.picker.visible).toBeTrue()
    expect(app.picker.title).toContain("Themes")
    expect(app.picker.select.options.length).toBeGreaterThan(20)
    expect(app.picker.select.options.some((option) => option.value === "theme:opencode")).toBeTrue()
  })

  test("executes a selected no-argument slash command on Enter and renders its result", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
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
    await setup.renderOnce()

    expect(app.state.transcript.at(-1)?.presentation).toBe("command_result")
    const commandCard = [...app.transcript.mountedCards.values()].at(-1)
    expect(commandCard?.header.plainText).toBe("Command result · /status")
    expect(commandCard?.markdown.content).toContain("actor idle · queue empty")
  })

  test("answers free-text questions through one contained composer-backed dock", async () => {
    const setup = await createTestRenderer({ width: 80, height: 10, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
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

  test("shows unknown status until confirmed and clears a friendly recovery banner on success", async () => {
    const setup = await createTestRenderer({ width: 100, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        connection: { phase: "disconnected", attempt: 7, error: null, gap: null },
      },
    })
    renderer.root.add(app)

    expect(app.banner.plainText).toBe("Connection lost · retrying…")
    expect(app.banner.plainText).not.toContain("attempt")
    expect(app.banner.plainText).not.toContain("disconnected")
    expect(app.statusLine.plainText).toContain("◉ execute")
    expect(app.statusLine.plainText).toContain("model —")
    expect(app.statusLine.plainText).toContain("cache —")
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

  test("prefills slash commands with required arguments instead of running invalid input", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    await setup.mockInput.typeText("/rew")
    expect(app.picker.select.getSelectedOption()?.value).toBe("rewind")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)

    expect(app.composer.value).toBe("/rewind ")
    expect(app.picker.visible).toBeFalse()
    expect(emitted.some((command) => command.type === "send_message")).toBeFalse()
  })

  test("keeps slash defaults and the full action palette useful before engine projections", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)

    await setup.mockInput.typeText("/")
    const slash = app.picker.select.options.map((option) => option.value)
    expect(slash).toContain("help")
    expect(slash).toContain("providers")
    expect(slash).toContain("agents")
    expect(slash).toContain("permissions")
    expect(slash.length).toBeGreaterThan(10)

    app.closePicker()
    app.openCommandPicker()
    const palette = app.picker.select.options.map((option) => option.value)
    expect(palette).toContain("session.list")
    expect(palette).toContain("provider.list")
    expect(palette).toContain("agent.children")
    expect(palette).toContain("mcp.configure")
    expect(palette).not.toContain("mcp.status")
    expect(palette).toContain("permissions.manage")
    expect(palette.length).toBeGreaterThan(10)

    const statusIndex = app.picker.select.options.findIndex(
      (option) => option.value === "status.show",
    )
    app.picker.select.setSelectedIndex(statusIndex)
    app.picker.select.selectCurrent()
    expect(app.composer.value).toBe("/status")
  })

  test("inspects a parent-owned child transcript and routes follow-ups without attaching its session", async () => {
    const setup = await createTestRenderer({ width: 96, height: 22, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openSubagentPicker()
    const list = emitted.find((command) => command.type === "list_subagents")
    expect(list?.type).toBe("list_subagents")
    app.handleEvent({
      type: "subagents_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: list!.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "parent-session",
      subagents: [{
        subagent_id: "child-one",
        child_session_id: "child-session",
        task: "Audit authentication",
        agent: "reviewer",
        model: "fast",
        isolation: "worktree",
        activity: "running",
      }],
    })
    expect(app.picker.select.options.map((option) => option.value)).toContain("child-one")
    app.picker.select.selectCurrent()
    await Bun.sleep(0)

    expect(app.activeSubagentId).toBe("child-one")
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "replay_subagent",
      session_id: "parent-session",
      subagent_id: "child-one",
      after_sequence: null,
    }))
    expect(emitted.some((command) => command.type === "resume_session")).toBeFalse()

    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-one",
      child_session_id: "child-session",
      child_sequence: "0",
      event: {
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "different-child-session",
          sequence_id: "1",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "forged-turn",
        text: "must not cross the child boundary",
      },
    })
    expect(app.visibleState.streamingTail).toBeNull()

    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-one",
      child_session_id: "child-session",
      child_sequence: "1",
      event: {
        type: "turn_started",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "1",
          emitted_at: "2026-01-01T00:00:01Z",
        },
        turn_id: "child-turn",
      },
    })
    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-one",
      child_session_id: "child-session",
      child_sequence: "2",
      event: {
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "2",
          emitted_at: "2026-01-01T00:00:02Z",
        },
        turn_id: "child-turn",
        text: "Authentication uses a bounded token exchange.",
      },
    })
    const replay = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: replay.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-one",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: replay.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-one",
      through_sequence: "1",
      next_cursor: null,
      tail_sequence: "1",
      has_more: false,
      events_before_page: "1",
      truncated: true,
    })
    expect(app.state.transcript).toEqual([])
    expect(app.visibleState.streamingTail?.text).toBe("Authentication uses a bounded token exchange.")
    expect(app.banner.plainText).toContain("Child · Audit authentication")
    expect(app.contextPanel.visible).toBeFalse()

    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-one",
      child_session_id: "child-session",
      child_sequence: "4",
      event: {
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "4",
          emitted_at: "2026-01-01T00:00:04Z",
        },
        turn_id: "child-turn",
        text: " Gap recovered.",
      },
    })
    const gapReplay = emitted.filter((command) => command.type === "replay_subagent").at(-1)!
    expect(gapReplay).toMatchObject({ after_sequence: "2" })
    expect(app.visibleState.streamingTail?.text).not.toContain("Gap recovered")
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: gapReplay.meta.request_id,
        emitted_at: "2026-01-01T00:00:04Z",
      },
      session_id: "parent-session",
      subagent_id: "child-one",
      child_session_id: "child-session",
      events: [{
        child_sequence: "3",
        event: {
          type: "thinking_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "3",
            emitted_at: "2026-01-01T00:00:03Z",
          },
          turn_id: "child-turn",
          text: "Recovered the missing broadcast.",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: gapReplay.meta.request_id,
        emitted_at: "2026-01-01T00:00:04Z",
      },
      session_id: "parent-session",
      subagent_id: "child-one",
      through_sequence: "3",
      next_cursor: null,
      tail_sequence: "3",
      has_more: false,
      events_before_page: "2",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toContain("Gap recovered.")

    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-one",
      child_session_id: "child-session",
      child_sequence: "5",
      event: {
        type: "tool_approval_needed",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "5",
          emitted_at: "2026-01-01T00:00:05Z",
        },
        turn_id: "child-turn",
        tool_call_id: "child-tool",
        name: "edit",
        args: { path: "src/auth.rs" },
        capabilities: ["write_filesystem"],
        rationale: "Update the child worktree",
      },
    })
    expect(app.banner.plainText).toContain("approval requested by child")
    expect(app.interactionPanel.visible).toBeFalse()

    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-one",
      child_session_id: "child-session",
      child_sequence: "6",
      event: {
        type: "turn_finished",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "6",
          emitted_at: "2026-01-01T00:00:06Z",
        },
        turn_id: "child-turn",
        status: "completed",
        usage: {
          input_tokens: "10",
          output_tokens: "5",
          cache_read_tokens: "0",
          cache_write_tokens: "0",
          reasoning_tokens: "0",
        },
        cost: { kind: "unavailable", reason: "fixture" },
      },
    })
    expect(app.composer.visible).toBeTrue()

    app.composer.value = "Check the refresh path too"
    expect(await app.composer.submit()).toBeTrue()
    expect(emitted.at(-1)).toMatchObject({
      type: "continue_subagent",
      session_id: "parent-session",
      subagent_id: "child-one",
      content: "Check the refresh path too",
    })

    app.openSubagentActionPicker()
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "inspect",
      "running",
      "interrupt",
      "close",
    ])
    app.picker.select.setSelectedIndex(3)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(emitted.at(-1)).toMatchObject({
      type: "list_subagents",
      session_id: "parent-session",
    })
    expect(emitted.at(-2)).toMatchObject({
      type: "close_subagent",
      session_id: "parent-session",
      subagent_id: "child-one",
    })
    expect(app.activeSubagentId).toBeNull()
    expect(app.state.subagents["child-one"]).toBeUndefined()
    expect(app.state.subagentOrder).not.toContain("child-one")
  })

  test("uses Escape to return to the parent and double Escape to interrupt a running child", async () => {
    const setup = await createTestRenderer({ width: 88, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
    expect(app.statusLine.plainText).toContain("INSERT")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.activeSubagentId).toBe("child-vim")
    expect(app.statusLine.plainText).toContain("NORMAL")
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

  test("keeps running child inspection read-only and guards follow-up submission", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-read-only",
        child_session_id: "child-session",
        task: "Keep inspection passive",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.activeSubagentId).toBe("child-read-only")
    expect(app.composer.visible).toBeFalse()
    expect(app.banner.plainText).toContain("read-only; interrupt to reply")

    app.composer.value = "This must not race the active child"
    expect(await app.composer.submit()).toBeFalse()
    expect(emitted.some((command) => command.type === "continue_subagent")).toBeFalse()
    expect(app.composer.value).toBe("This must not race the active child")
    expect(app.banner.plainText).toContain("interrupt it before sending a follow-up")

    const replay = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: replay.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-read-only",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }, {
        child_sequence: "2",
        event: {
          type: "turn_finished",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "2",
            emitted_at: "2026-01-01T00:00:02Z",
          },
          turn_id: "child-turn",
          status: "completed",
          usage: {
            input_tokens: "10",
            output_tokens: "5",
            cache_read_tokens: "0",
            cache_write_tokens: "0",
            reasoning_tokens: "0",
          },
          cost: { kind: "unavailable", reason: "fixture" },
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: replay.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-read-only",
      through_sequence: "2",
      next_cursor: null,
      tail_sequence: "2",
      has_more: false,
      events_before_page: "1",
      truncated: true,
    })
    expect(app.composer.visible).toBeTrue()
    expect(await app.composer.submit()).toBeTrue()
    expect(emitted.at(-1)).toMatchObject({
      type: "continue_subagent",
      subagent_id: "child-read-only",
      content: "This must not race the active child",
    })
  })

  test("keeps child shell routing, status, and picker bounds truthful", async () => {
    const setup = await createTestRenderer({ width: 52, height: 10, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      terminalHandover: { suspend() {}, resume() {} },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()
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
        subagent_id: "child-bounded",
        child_session_id: "child-session",
        task: `Bounded ${"task ".repeat(300)}`,
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    expect(Number(app.picker.height)).toBeLessThanOrEqual(7)
    expect(app.picker.select.options[0]?.name.length).toBeLessThanOrEqual(512)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)

    const replay = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: replay.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "parent-session",
      subagent_id: "child-bounded",
      through_sequence: null,
      next_cursor: null,
      tail_sequence: null,
      has_more: false,
      events_before_page: "0",
      truncated: false,
    })
    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-bounded",
      child_session_id: "child-session",
      child_sequence: "1",
      event: {
        type: "context_usage_updated",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "1",
          emitted_at: "2026-01-01T00:00:01Z",
        },
        turn_id: "child-turn",
        used_tokens: "2000",
        usable_tokens: "10000",
        reserved_tokens: "1000",
        stable_prefix_hash: "fixture",
        cache_hit_basis_points: 0,
      },
    })
    expect(app.statusLine.plainText).toContain("ctx 2.0k/10k")
    app.openSubagentActionPicker()
    expect(Number(app.picker.height)).toBeLessThanOrEqual(7)
    app.closePicker()
    expect(app.statusLine.plainText).toContain("ctx 2.0k/10k")

    app.composer.value = "!pwd"
    expect(await app.composer.submit()).toBeTrue()
    expect(emitted.at(-1)).toMatchObject({
      type: "user_shell_started",
      session_id: "parent-session",
      command: "pwd",
    })
    expect(emitted.at(-1)?.type).not.toBe("continue_subagent")
    expect(app.activeSubagentId).toBeNull()
  })

  test("keeps child-list failures retryable instead of claiming the list is empty", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    let attempts = 0
    const app = createRottweilerApp(renderer, {
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

  test("retains live child progress when replay fails and converges through a cursor replay", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const replayResolvers: Array<(outcome: CommandOutcome) => void> = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        if (command.type === "replay_subagent") {
          return new Promise<CommandOutcome>((resolve) => replayResolvers.push(resolve))
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
      subagents: [{
        subagent_id: "child-recovery",
        child_session_id: "child-session",
        task: "Recover broadcasts",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)

    for (const sequence of [1, 2]) {
      app.handleEvent({
        type: "subagent_progress",
        parent_session_id: "parent-session",
        subagent_id: "child-recovery",
        child_session_id: "child-session",
        child_sequence: String(sequence),
        event: sequence === 1
          ? {
              type: "turn_started",
              meta: {
                protocol_version: PROTOCOL_VERSION,
                session_id: "child-session",
                sequence_id: "1",
                emitted_at: "2026-01-01T00:00:01Z",
              },
              turn_id: "child-turn",
            }
          : {
              type: "text_delta",
              meta: {
                protocol_version: PROTOCOL_VERSION,
                session_id: "child-session",
                sequence_id: "2",
                emitted_at: "2026-01-01T00:00:02Z",
              },
              turn_id: "child-turn",
              text: "Buffered before failure. ",
            },
      })
    }
    replayResolvers[0]?.({
      type: "rejected",
      error: {
        category: "protocol",
        code: "replay_failed",
        message: "temporary replay failure",
        retryable: true,
      },
    })
    await Bun.sleep(0)
    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-recovery",
      child_session_id: "child-session",
      child_sequence: "3",
      event: {
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "3",
          emitted_at: "2026-01-01T00:00:03Z",
        },
        turn_id: "child-turn",
        text: "Still retained.",
      },
    })
    const replays = emitted.filter((command) => command.type === "replay_subagent")
    expect(replays).toHaveLength(2)
    expect(replays[1]).toMatchObject({ after_sequence: null })
    const recovery = replays[1]!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: recovery.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "parent-session",
      subagent_id: "child-recovery",
      child_session_id: "child-session",
      events: [
        {
          child_sequence: "1",
          event: {
            type: "turn_started",
            meta: {
              protocol_version: PROTOCOL_VERSION,
              session_id: "child-session",
              sequence_id: "1",
              emitted_at: "2026-01-01T00:00:01Z",
            },
            turn_id: "child-turn",
          },
        },
        {
          child_sequence: "2",
          event: {
            type: "text_delta",
            meta: {
              protocol_version: PROTOCOL_VERSION,
              session_id: "child-session",
              sequence_id: "2",
              emitted_at: "2026-01-01T00:00:02Z",
            },
            turn_id: "child-turn",
            text: "Buffered before failure. ",
          },
        },
      ],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: recovery.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "parent-session",
      subagent_id: "child-recovery",
      through_sequence: "2",
      next_cursor: null,
      tail_sequence: "2",
      has_more: false,
      events_before_page: "1",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toBe(
      "Buffered before failure. Still retained.",
    )
    replayResolvers[1]?.({ type: "accepted" })
  })

  test("ignores a stale replay rejection without deleting the newer replay correlation", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const replayResolvers: Array<(outcome: CommandOutcome) => void> = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        if (command.type === "replay_subagent") {
          return new Promise<CommandOutcome>((resolve) => replayResolvers.push(resolve))
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
      subagents: [{
        subagent_id: "child-race",
        child_session_id: "child-session",
        task: "Replay race",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    app.openSubagentActionPicker("child-race")
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const replays = emitted.filter((command) => command.type === "replay_subagent")
    expect(replays).toHaveLength(2)

    replayResolvers[0]?.({
      type: "rejected",
      error: {
        category: "protocol",
        code: "stale",
        message: "stale replay rejected",
        retryable: true,
      },
    })
    await Bun.sleep(0)
    const current = replays[1]!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: current.meta.request_id,
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "parent-session",
      subagent_id: "child-race",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }, {
        child_sequence: "2",
        event: {
          type: "text_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "2",
            emitted_at: "2026-01-01T00:00:02Z",
          },
          turn_id: "child-turn",
          text: "Newest replay survived.",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: current.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-race",
      through_sequence: "2",
      next_cursor: null,
      tail_sequence: "2",
      has_more: false,
      events_before_page: "1",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toBe("Newest replay survived.")
    replayResolvers[1]?.({ type: "accepted" })
  })

  test("reissues an accepted child replay after reconnect and preserves buffered progress", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-reconnect-accepted",
        child_session_id: "child-session",
        task: "Reconnect after acceptance",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const first = emitted.find((command) => command.type === "replay_subagent")!

    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-reconnect-accepted",
      child_session_id: "child-session",
      child_sequence: "2",
      event: {
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "2",
          emitted_at: "2026-01-01T00:00:02Z",
        },
        turn_id: "child-turn",
        text: "Buffered across reconnect.",
      },
    })
    completeTransportReconnect(app)
    const replays = emitted.filter((command) => command.type === "replay_subagent")
    expect(replays).toHaveLength(2)
    const recovered = replays[1]!
    expect(recovered).toMatchObject({ after_sequence: null })
    expect(recovered.meta.request_id).not.toBe(first.meta.request_id)

    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-accepted",
      through_sequence: null,
      next_cursor: null,
      tail_sequence: null,
      has_more: false,
      events_before_page: "0",
      truncated: false,
    })
    expect(app.banner.plainText).toContain("loading transcript")
    expect(emitted.filter((command) => command.type === "replay_subagent")).toHaveLength(2)

    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: recovered.meta.request_id,
        emitted_at: "2026-01-01T00:00:04Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-accepted",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: recovered.meta.request_id,
        emitted_at: "2026-01-01T00:00:04Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-accepted",
      through_sequence: "1",
      next_cursor: null,
      tail_sequence: "1",
      has_more: false,
      events_before_page: "1",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toBe("Buffered across reconnect.")
  })

  test("reissues child replay from the verified cursor when reconnect interrupts a page", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-reconnect-batch",
        child_session_id: "child-session",
        task: "Reconnect between batch and completion",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const first = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-batch",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }, {
        child_sequence: "2",
        event: {
          type: "text_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "2",
            emitted_at: "2026-01-01T00:00:02Z",
          },
          turn_id: "child-turn",
          text: "Applied before reconnect.",
        },
      }],
    })

    completeTransportReconnect(app)
    const recovered = emitted.filter((command) => command.type === "replay_subagent").at(-1)!
    expect(recovered).toMatchObject({ after_sequence: "2" })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-batch",
      through_sequence: "2",
      next_cursor: null,
      tail_sequence: "2",
      has_more: false,
      events_before_page: "1",
      truncated: true,
    })
    expect(app.banner.plainText).toContain("loading transcript")
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: recovered.meta.request_id,
        emitted_at: "2026-01-01T00:00:04Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-batch",
      through_sequence: "2",
      next_cursor: null,
      tail_sequence: "2",
      has_more: false,
      events_before_page: "2",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toBe("Applied before reconnect.")
    expect(emitted.filter((command) => command.type === "replay_subagent")).toHaveLength(2)
  })

  test("replaces the pending next-page request when reconnect occurs between replay pages", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-reconnect-pages",
        child_session_id: "child-session",
        task: "Reconnect between pages",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const first = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-pages",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }, {
        child_sequence: "2",
        event: {
          type: "text_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "2",
            emitted_at: "2026-01-01T00:00:02Z",
          },
          turn_id: "child-turn",
          text: "Page one. ",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-pages",
      through_sequence: "2",
      next_cursor: "2",
      tail_sequence: "3",
      has_more: true,
      events_before_page: "1",
      truncated: true,
    })
    const second = emitted.filter((command) => command.type === "replay_subagent").at(-1)!
    expect(second).toMatchObject({ after_sequence: "2" })

    completeTransportReconnect(app)
    const third = emitted.filter((command) => command.type === "replay_subagent").at(-1)!
    expect(third).toMatchObject({ after_sequence: "2" })
    expect(third.meta.request_id).not.toBe(second.meta.request_id)

    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: second.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-pages",
      child_session_id: "child-session",
      events: [{
        child_sequence: "3",
        event: {
          type: "text_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "3",
            emitted_at: "2026-01-01T00:00:03Z",
          },
          turn_id: "child-turn",
          text: "Stale page must be ignored.",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: second.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-pages",
      through_sequence: "3",
      next_cursor: null,
      tail_sequence: "3",
      has_more: false,
      events_before_page: "2",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toBe("Page one. ")

    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: third.meta.request_id,
        emitted_at: "2026-01-01T00:00:04Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-pages",
      child_session_id: "child-session",
      events: [{
        child_sequence: "3",
        event: {
          type: "text_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "3",
            emitted_at: "2026-01-01T00:00:04Z",
          },
          turn_id: "child-turn",
          text: "Page two.",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: third.meta.request_id,
        emitted_at: "2026-01-01T00:00:04Z",
      },
      session_id: "parent-session",
      subagent_id: "child-reconnect-pages",
      through_sequence: "3",
      next_cursor: null,
      tail_sequence: "3",
      has_more: false,
      events_before_page: "2",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toBe("Page one. Page two.")
    expect(emitted.filter((command) => command.type === "replay_subagent")).toHaveLength(3)
  })

  test("restores a rejected child submission only to its originating child draft", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let rejectFollowUp: ((outcome: CommandOutcome) => void) | undefined
    let request = 0
    const app = createRottweilerApp(renderer, {
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
      sessionId: "parent-session",
      requestId: () => `request-${++request}`,
      terminalHandover: { suspend() {}, resume() {} },
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

  test("bounds buffered child broadcasts by bytes and restarts replay from the durable cursor", async () => {
    const setup = await createTestRenderer({ width: 60, height: 12, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-bytes",
        child_session_id: "child-session",
        task: "Bound broadcast memory",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const firstReplay = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-bytes",
      child_session_id: "child-session",
      child_sequence: "1",
      event: {
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "1",
          emitted_at: "2026-01-01T00:00:01Z",
        },
        turn_id: "child-turn",
        text: "x".repeat(8 * 1024 * 1024),
      },
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: firstReplay.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-bytes",
      through_sequence: null,
      next_cursor: null,
      tail_sequence: null,
      has_more: false,
      events_before_page: "0",
      truncated: false,
    })
    const replays = emitted.filter((command) => command.type === "replay_subagent")
    expect(replays).toHaveLength(2)
    expect(replays[1]).toMatchObject({ after_sequence: null })
  })

  test("paginates child replay to a stable tail before draining buffered live progress", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-pages",
        child_session_id: "child-session",
        task: "Page replay",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const first = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_progress",
      parent_session_id: "parent-session",
      subagent_id: "child-pages",
      child_session_id: "child-session",
      child_sequence: "4",
      event: {
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "child-session",
          sequence_id: "4",
          emitted_at: "2026-01-01T00:00:04Z",
        },
        turn_id: "child-turn",
        text: "live progress",
      },
    })
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "parent-session",
      subagent_id: "child-pages",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }, {
        child_sequence: "2",
        event: {
          type: "text_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "2",
            emitted_at: "2026-01-01T00:00:02Z",
          },
          turn_id: "child-turn",
          text: "page one; ",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-pages",
      through_sequence: "2",
      next_cursor: "2",
      tail_sequence: "3",
      has_more: true,
      events_before_page: "1",
      truncated: true,
    })
    const second = emitted.filter((command) => command.type === "replay_subagent").at(-1)!
    expect(second).toMatchObject({ after_sequence: "2" })
    expect(app.visibleState.streamingTail?.text).toBe("page one; ")
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: second.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "parent-session",
      subagent_id: "child-pages",
      child_session_id: "child-session",
      events: [{
        child_sequence: "3",
        event: {
          type: "text_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "3",
            emitted_at: "2026-01-01T00:00:03Z",
          },
          turn_id: "child-turn",
          text: "page two; ",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: second.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "parent-session",
      subagent_id: "child-pages",
      through_sequence: "3",
      next_cursor: null,
      tail_sequence: "3",
      has_more: false,
      events_before_page: "2",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toBe("page one; page two; live progress")
    expect(emitted.filter((command) => command.type === "replay_subagent")).toHaveLength(2)
  })

  test("rejects a nonadvancing child replay cursor and retries loudly from the verified event", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-cursor",
        child_session_id: "child-session",
        task: "Validate cursors",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const first = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "parent-session",
      subagent_id: "child-cursor",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "parent-session",
      subagent_id: "child-cursor",
      through_sequence: "1",
      next_cursor: "1",
      tail_sequence: "3",
      has_more: true,
      events_before_page: "1",
      truncated: true,
    })
    const second = emitted.filter((command) => command.type === "replay_subagent").at(-1)!
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: second.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "parent-session",
      subagent_id: "child-cursor",
      through_sequence: null,
      next_cursor: "1",
      tail_sequence: "3",
      has_more: true,
      events_before_page: "1",
      truncated: true,
    })
    const replays = emitted.filter((command) => command.type === "replay_subagent")
    expect(replays).toHaveLength(3)
    expect(replays.at(-1)).toMatchObject({ after_sequence: "1" })
    expect(app.banner.plainText).toContain("invalid next-page cursor")
  })

  test("retries loudly when a final child replay page stops before its declared tail", async () => {
    const setup = await createTestRenderer({ width: 80, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-tail",
        child_session_id: "child-session",
        task: "Verify replay tail",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "running",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const first = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "parent-session",
      subagent_id: "child-tail",
      child_session_id: "child-session",
      events: [{
        child_sequence: "1",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "1",
            emitted_at: "2026-01-01T00:00:01Z",
          },
          turn_id: "child-turn",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: first.meta.request_id,
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "parent-session",
      subagent_id: "child-tail",
      through_sequence: "1",
      next_cursor: null,
      tail_sequence: "2",
      has_more: false,
      events_before_page: "1",
      truncated: true,
    })
    const replays = emitted.filter((command) => command.type === "replay_subagent")
    expect(replays).toHaveLength(2)
    expect(replays.at(-1)).toMatchObject({ after_sequence: "1" })
    expect(app.banner.plainText).toContain("stopped before its durable tail")
  })

  test("labels an intentional initial child replay tail without claiming data loss", async () => {
    const setup = await createTestRenderer({ width: 92, height: 16, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
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
        subagent_id: "child-recent",
        child_session_id: "child-session",
        task: "Recent retained history",
        agent: "reviewer",
        model: "fast",
        isolation: "shared",
        activity: "idle",
      }],
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const replay = emitted.find((command) => command.type === "replay_subagent")!
    app.handleEvent({
      type: "subagent_replay_batch",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: replay.meta.request_id,
        emitted_at: "2026-01-01T00:00:09Z",
      },
      session_id: "parent-session",
      subagent_id: "child-recent",
      child_session_id: "child-session",
      events: [{
        child_sequence: "9",
        event: {
          type: "turn_started",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "9",
            emitted_at: "2026-01-01T00:00:09Z",
          },
          turn_id: "child-turn",
        },
      }, {
        child_sequence: "10",
        event: {
          type: "text_delta",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            session_id: "child-session",
            sequence_id: "10",
            emitted_at: "2026-01-01T00:00:10Z",
          },
          turn_id: "child-turn",
          text: "Recent retained work.",
        },
      }],
    })
    app.handleEvent({
      type: "subagent_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: replay.meta.request_id,
        emitted_at: "2026-01-01T00:00:10Z",
      },
      session_id: "parent-session",
      subagent_id: "child-recent",
      through_sequence: "10",
      next_cursor: null,
      tail_sequence: "10",
      has_more: false,
      events_before_page: "9",
      truncated: true,
    })
    expect(app.visibleState.streamingTail?.text).toBe("Recent retained work.")
    expect(app.banner.plainText).toContain("Showing recent activity; 9 earlier events retained")
    expect(app.banner.plainText).not.toContain("data loss")
    expect(emitted.filter((command) => command.type === "replay_subagent")).toHaveLength(1)
  })

  test("searches settings actions and never one-clicks destructive choices", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        commands: [{ name: "mcp", description: "Manage MCP servers", usage: "[status]" }],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.mockInput.typeText("mcp")
    expect(app.picker.select.options.map((option) => option.value)).toContain("mcp.manage")

    app.picker.input.value = "trust this folder"
    const trustIndex = app.picker.select.options.findIndex(
      (option) => option.value === "trust.grant",
    )
    expect(trustIndex).toBeGreaterThanOrEqual(0)
    app.picker.select.setSelectedIndex(trustIndex)
    app.picker.select.selectCurrent()
    expect(app.composer.value).toBe("/trust grant ")
    expect(emitted).toHaveLength(1)
    expect(emitted[0]?.type).toBe("list_commands")
  })

  test("refreshes live catalogs when pickers reopen and workspace roots change", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        commands: [{ name: "first", description: "First", usage: "" }],
        models: [{ alias: "fast", providers: ["openai"], vision: false, thinking: false, toolCalling: true }],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    const firstCatalogRequest = emitted.find((command) => command.type === "list_commands")
    expect(firstCatalogRequest?.type).toBe("list_commands")
    app.handleEvent({
      type: "command_descriptors_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: firstCatalogRequest!.meta.request_id, emitted_at: "2026-01-01T00:00:00Z" },
      session_id: "session-local",
      commands: [{ name: "second", description: "Second", usage: "" }],
      truncated: false,
    })
    app.closePicker()
    app.openCommandPicker()
    expect(emitted.filter((command) => command.type === "list_commands")).toHaveLength(2)

    app.handleEvent({
      type: "command_finished",
      meta: { protocol_version: PROTOCOL_VERSION, session_id: "session-local", sequence_id: "1", emitted_at: "2026-01-01T00:00:01Z" },
      name: "add-dir",
      message: "added workspace root @root/1",
      unrestorable_paths: [],
    })
    expect(emitted.filter((command) => command.type === "list_commands")).toHaveLength(3)

    app.openModelPicker()
    const firstModelsRequest = emitted.find((command) => command.type === "list_models")
    expect(firstModelsRequest?.type).toBe("list_models")
    app.handleEvent({
      type: "models_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "ui", request_id: firstModelsRequest!.meta.request_id, emitted_at: "2026-01-01T00:00:02Z" },
      models: [],
    })
    app.closePicker()
    app.openModelPicker()
    expect(emitted.filter((command) => command.type === "list_models")).toHaveLength(2)
  })

  test("shows command catalog truncation in both command surfaces", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        commands: [{ name: "fixture", description: "Fixture", usage: "" }],
        commandsTruncated: true,
      },
    })
    renderer.root.add(app)
    await setup.mockInput.typeText("/")
    expect(app.picker.title).toContain("results truncated")
    app.closePicker()
    app.openCommandPicker()
    expect(app.picker.select.options.map((option) => option.value)).toContain("commands.truncated")
  })

  test("keeps local slash commands usable while a rejected live catalog is loud and retryable", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    let attempts = 0
    const app = createRottweilerApp(renderer, {
      onCommand(command) {
        if (command.type !== "list_commands") return { type: "accepted" }
        attempts += 1
        return {
          type: "rejected",
          error: {
            category: "protocol",
            code: "catalog_unavailable",
            message: "driver lease rejected the command catalog",
            retryable: true,
          },
        }
      },
    })
    renderer.root.add(app)

    await setup.mockInput.typeText("/")
    await Bun.sleep(0)

    expect(app.picker.select.options.map((option) => option.value)).toContain("commands.error")
    expect(app.picker.select.options.map((option) => option.value)).toContain("help")
    expect(app.picker.select.options[0]?.description).toContain(
      "driver lease rejected the command catalog",
    )
    expect(app.banner.plainText).toContain("couldn't load commands")

    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(attempts).toBe(2)
  })

  test("ignores late projection failures and engine events after OpenTUI destroys the application", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    let finishProjection!: (outcome: CommandOutcome) => void
    const deferredProjection = new Promise<CommandOutcome>((resolve) => {
      finishProjection = resolve
    })
    let postDestroyCommands = 0
    const app = createRottweilerApp(renderer, {
      onCommand(command) {
        if (command.type === "list_commands") return deferredProjection
        postDestroyCommands += 1
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    renderer.destroy()
    renderer = undefined
    finishProjection({
      type: "rejected",
      error: {
        category: "protocol",
        code: "runtime_stopped",
        message: "the projection was cancelled during teardown",
        retryable: true,
      },
    })
    await Bun.sleep(0)

    expect(app.state.errors).toHaveLength(0)

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
    expect(app.state.transcript).toHaveLength(0)
    expect(postDestroyCommands).toBe(0)
  })

  test("renders model projection failures in both model and provider pickers", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      onCommand(command) {
        if (command.type !== "list_models") return { type: "accepted" }
        return Promise.reject(new Error("provider discovery timed out"))
      },
    })
    renderer.root.add(app)

    app.openModelPicker()
    await Bun.sleep(0)
    expect(app.picker.select.options[0]?.value).toBe("models.error")
    expect(app.picker.select.options[0]?.description).toContain("provider discovery timed out")

    app.closePicker()
    app.openProviderPicker()
    await Bun.sleep(0)
    expect(app.picker.select.options[0]?.value).toBe("providers.error")
    expect(app.picker.select.options[0]?.description).toContain("provider discovery timed out")
  })

  test("presents model and provider loading as non-selectable picker status", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)

    app.openProviderPicker()
    expect(app.picker.status.plainText).toContain("Loading provider connections")
    expect(app.picker.status.visible).toBeTrue()
    expect(app.picker.select.visible).toBeFalse()
    expect(app.picker.select.options).toHaveLength(0)
    app.picker.select.selectCurrent()
    expect(app.state.errors).toHaveLength(0)

    app.openModelPicker()
    expect(app.picker.status.plainText).toContain("Loading available models")
    expect(app.picker.select.visible).toBeFalse()
    expect(app.picker.select.options).toHaveLength(0)
    app.picker.select.selectCurrent()
    expect(app.state.errors).toHaveLength(0)
  })

  test("clears a partial anchored trigger before opening a local slash action", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        models: [{ alias: "fast", providers: ["openai"], vision: true, thinking: true, toolCalling: true }],
      },
    })
    renderer.root.add(app)
    await setup.mockInput.typeText("/model")
    app.picker.select.selectCurrent()
    await setup.renderOnce()

    expect(app.composer.value).toBe("")
    expect(app.picker.title).toContain("Models")
  })

  test("scrolls the Ctrl-P viewport without moving selection and activates the exact mouse row", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        commands: Array.from({ length: 12 }, (_, index) => ({
          name: `command-${index}`,
          description: `Command ${index}`,
          usage: `/command-${index}`,
        })),
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.renderOnce()

    const offset = () =>
      (app.picker.select as unknown as { scrollOffset: number }).scrollOffset
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    expect(offset()).toBe(0)
    await setup.mockMouse.scroll(app.picker.select.x + 2, app.picker.select.y + 1, "down")
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    expect(offset()).toBe(1)
    await setup.mockMouse.click(app.picker.select.x + 2, app.picker.select.y)
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.title).toContain("Models")
  })

  test("centers Ctrl-P keyboard selection instead of following viewport edges", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        commands: Array.from({ length: 30 }, (_, index) => ({
          name: `command-${index}`,
          description: `Command ${index}`,
          usage: `/command-${index}`,
        })),
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.renderOnce()

    const offset = () =>
      (app.picker.select as unknown as { scrollOffset: number }).scrollOffset
    expect(app.picker.select.showDescription).toBeFalse()
    const visible = Math.max(1, Math.floor(app.picker.select.height))
    const maximum = app.picker.select.options.length - visible
    for (let index = 1; index <= visible + 2; index += 1) {
      setup.mockInput.pressArrow("down")
      expect(app.picker.select.getSelectedIndex()).toBe(index)
      expect(offset()).toBe(Math.min(maximum, Math.max(0, index - Math.floor(visible / 2))))
    }
    setup.mockInput.pressArrow("up")
    const previous = visible + 1
    expect(app.picker.select.getSelectedIndex()).toBe(previous)
    expect(offset()).toBe(
      Math.min(maximum, Math.max(0, previous - Math.floor(visible / 2))),
    )
    setup.mockInput.pressKey("HOME")
    expect(offset()).toBe(0)
    setup.mockInput.pressKey("END")
    expect(offset()).toBe(maximum)
  })

  test("offers exact model-provider route switching through typed pickers", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        models: [
          { alias: "fast", providers: ["openai", "copilot"], vision: true, thinking: true, toolCalling: true },
          { alias: "steady", providers: ["copilot"], vision: false, thinking: true, toolCalling: true },
        ],
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.composer.value = "/providers"
    expect(await app.composer.submit()).toBeTrue()
    expect(app.picker.title).toContain("Providers")
    expect(app.picker.select.options.map((option) => option.value)).toEqual(["copilot", "openai"])
    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Models · copilot")
    expect(app.picker.select.options.map((option) => option.value)).toEqual(["fast", "steady"])
    app.picker.select.setSelectedIndex(1)
    app.picker.select.selectCurrent()
    expect(commands).toContainEqual(expect.objectContaining({
      type: "switch_model",
      model: "steady",
      provider: "copilot",
    }))
    app.handleEvent({
      type: "model_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      model: "steady",
      provider: "copilot",
    })
    expect(app.state.provider).toBe("copilot")
    expect(app.statusLine.plainText).toContain("model copilot/steady")

    app.composer.value = "/models"
    expect(await app.composer.submit()).toBeTrue()
    expect(app.picker.title).toContain("Models")
    expect(app.picker.select.options.map((option) => option.value)).toEqual(["fast", "steady"])
  })

  test("uses provider inventory, concrete models, command sources, and persisted settings", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        commands: [{
          name: "deploy",
          description: "Deploy project",
          usage: "/deploy",
          source: "project",
        }],
        models: [{
          alias: "copilot/gpt-5",
          id: "copilot/gpt-5",
          displayName: "GPT-5",
          provider: "copilot",
          providers: ["copilot"],
          aliases: ["fast"],
          current: true,
          available: true,
          status: null,
          vision: true,
          thinking: true,
          toolCalling: true,
        }],
        providers: [{
          name: "copilot",
          authKind: "device_flow",
          nextAction: "select_models",
          configured: true,
          authenticated: false,
          reachable: false,
          modelCount: 0,
          status: "login required",
        }],
        settings: [
          {
            key: "ui.theme",
            label: "Theme",
            value: "opencode",
            choices: ["system", "opencode", "tokyonight"],
            provenance: "built-in",
            appliesImmediately: false,
          },
          {
            key: "models.thinking.fast",
            label: "Thinking · fast",
            value: "medium",
            choices: ["off", "low", "medium", "high"],
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "permissions.default",
            label: "Default permission",
            value: "ask",
            choices: ["ask", "allow", "deny"],
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "compaction.auto",
            label: "Automatic compaction",
            value: "true",
            choices: ["true", "false"],
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "ui.keybindings.preset",
            label: "Keybinding preset",
            value: "standard",
            choices: ["standard", "vim"],
            provenance: "user keybindings",
            appliesImmediately: false,
          },
          {
            key: "mcp.servers.docs.enabled",
            label: "MCP · docs",
            value: "true",
            choices: ["true", "false"],
            provenance: "user MCP configuration",
            appliesImmediately: false,
          },
        ],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openProviderPicker()
    expect(app.picker.select.options.map((option) => option.value)).toEqual(["copilot"])
    expect(app.picker.select.options[0]?.description).toContain("Sign in with GitHub")

    app.openModelPicker()
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "copilot/gpt-5",
    ])
    expect(app.picker.select.options.map((option) => option.name).join(" ")).not.toContain(
      "Alias ·",
    )
    expect(app.picker.select.options.map((option) => option.name).join(" ")).not.toContain(
      "Thinking ·",
    )
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "switch_model",
      model: "copilot/gpt-5",
      provider: "copilot",
    }))
    const concreteSwitch = emitted.find(
      (command) => command.type === "switch_model" && command.model === "copilot/gpt-5",
    )
    expect(concreteSwitch).toBeDefined()
    app.handleEvent({
      type: "model_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
        caused_by: concreteSwitch?.meta.request_id,
      },
      model: "copilot/gpt-5",
      provider: "copilot",
    })
    expect(app.statusLine.plainText).toContain("model copilot/gpt-5")
    expect(app.statusLine.plainText).not.toContain("copilot/copilot")
    app.handleEvent({
      type: "conversation_turn_committed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z"
      },
      agent_turn: "1",
      turn: {
        role: "assistant",
        blocks: [{ type: "text", text: "fallback response" }],
        meta: {
          model: "openai/gpt-5-fallback",
          synthetic: false,
          summary: false
        }
      }
    })
    expect(app.state.provider).toBe("openai")
    expect(app.state.model).toBe("openai/gpt-5-fallback")
    expect(app.statusLine.plainText).toContain("model openai/gpt-5-fallback")
    expect(app.statusLine.plainText).not.toContain("openai/openai")
    expect(emitted).not.toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "project.models.default",
    }))

    expect(app.picker.input.isDestroyed).toBeFalse()
    app.openSettingsPicker()
    const settingOptions = app.picker.select.options.map((option) => option.value)
    expect(settingOptions).toContain("models.thinking.fast:high")
    expect(settingOptions).toContain("permissions.default:deny")
    expect(settingOptions).toContain("compaction.auto:false")
    expect(settingOptions).toContain("ui.keybindings.preset:vim")
    expect(settingOptions).toContain("mcp.servers.docs.enabled:false")
    const tokyoNight = app.picker.select.options.findIndex(
      (option) => option.value === "ui.theme:tokyonight",
    )
    app.picker.select.setSelectedIndex(tokyoNight)
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "ui.theme",
      value: "tokyonight",
    }))

    app.closePicker()
    await setup.mockInput.typeText("/")
    const projectCommand = app.picker.select.options.find(
      (option) => option.value === "deploy",
    )
    expect(projectCommand?.description).toContain("Project · Deploy project")
    app.closePicker()
    app.openCommandPicker()
    const paletteCommand = app.picker.select.options.find(
      (option) => option.value === "slash.deploy",
    )
    expect(paletteCommand?.description).toContain("Project · Deploy project")
  })

  test("reviews, confirms, and enables a live MCP server through typed commands", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openMcpPicker()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({ type: "list_mcp_servers" }))
    app.picker.select.selectCurrent()
    await setup.mockInput.typeText("docs.remote")
    setup.mockInput.pressEnter()
    await setup.mockInput.typeText("https://example.com/mcp")
    setup.mockInput.pressEnter()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "add_mcp_http_server",
      name: "docs.remote",
      endpoint: "https://example.com/mcp",
    }))

    app.handleEvent({
      type: "mcp_servers_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: "mcp-list",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      servers: [{
        name: "docs.remote",
        enabled: false,
        approved: false,
        state: { type: "approval_required" },
        tool_count: 0,
        resource_count: 0,
        prompt_count: 0,
      }],
    })
    const reviewIndex = app.picker.select.options.findIndex(
      (option) => option.value === "mcp.review.docs.remote",
    )
    expect(app.picker.select.options[reviewIndex]?.description).toContain("Approval needed")
    expect(app.picker.select.options[reviewIndex]?.description).not.toContain("approval_required")
    app.picker.select.setSelectedIndex(reviewIndex)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "review_mcp_server",
      name: "docs.remote",
    }))

    const fingerprint = "a".repeat(64)
    app.handleEvent({
      type: "mcp_server_approval_reviewed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: "mcp-review",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "session-local",
      review: {
        server: "docs.remote",
        transport: "streamable_http",
        endpoint: "https://example.com/mcp",
        origin: "user",
        defer_tools: true,
        fingerprint,
        previously_approved: false,
      },
    })
    const approveIndex = app.picker.select.options.findIndex(
      (option) => option.value === "mcp.approve.docs.remote",
    )
    expect(app.picker.select.options[approveIndex]?.description).toContain(fingerprint)
    expect(app.picker.select.options[approveIndex]?.description).toContain("Remote HTTPS")
    expect(app.picker.select.options[approveIndex]?.description).not.toContain("streamable_http")
    app.picker.select.setSelectedIndex(approveIndex)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "approve_mcp_server",
      name: "docs.remote",
      fingerprint,
    }))

    app.handleEvent({
      type: "mcp_servers_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: "mcp-approved",
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "session-local",
      servers: [{
        name: "docs.remote",
        enabled: false,
        approved: true,
        state: { type: "disabled" },
        tool_count: 0,
        resource_count: 0,
        prompt_count: 0,
      }],
    })
    const enableIndex = app.picker.select.options.findIndex(
      (option) => option.value === "mcp.toggle.docs.remote",
    )
    app.picker.select.setSelectedIndex(enableIndex)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "set_mcp_server_enabled",
      name: "docs.remote",
      enabled: true,
    }))
    expect(emitted.some((command) => command.type === "send_message")).toBe(false)
  })

  test("manages typed permission rows without transcript JSON or manual ids", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(setup.renderer, {
      sessionId: "session-permissions",
      clientId: "permission-driver",
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    setup.renderer.root.add(app)

    app.composer.value = "preserved draft"
    app.openPermissionPicker()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "list_permissions",
      session_id: "session-permissions",
    }))
    expect(app.picker.status.plainText).toContain("Loading permission rules")
    expect(app.picker.select.visible).toBeFalse()
    expect(app.picker.select.options).toHaveLength(0)
    setup.mockInput.pressEnter()
    await setup.mockInput.typeText("hidden input")
    await setup.mockInput.pasteBracketedText("hidden paste")
    expect(app.composer.value).toBe("preserved draft")
    expect(emitted.filter((command) => command.type === "list_permissions")).toHaveLength(1)
    app.handleEvent({
      type: "permissions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "permission-driver",
        request_id: "permission-list",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-permissions",
      permissions: {
        default: "ask",
        effective_rules: [{ id: "effective:one", pattern: "bash(rm *)", action: "deny" }],
        project_rules: [],
        session_rules: [{ id: "session:one", pattern: "bash(cargo test*)", action: "ask" }],
        approvals: [{
          id: "session:opaque-approval",
          scope: "session",
          tool_name: "read",
          summary: "exact-invocation=hidden capabilities=ReadFilesystem approval=none",
        }],
        truncated: false,
      },
    })
    expect(app.picker.select.options.map((option) => option.value)).toContain(
      "permissions.effective.effective:one",
    )
    expect(app.picker.status.visible).toBeFalse()
    expect(app.picker.select.visible).toBeTrue()
    const permissionCopy = app.picker.select.options
      .flatMap((option) => [option.name, option.description])
      .join("\n")
    expect(permissionCopy).not.toContain("Session-scoped")
    expect(permissionCopy).not.toContain("tool(argument")
    expect(permissionCopy).not.toContain("exact-invocation")

    const removeIndex = app.picker.select.options.findIndex(
      (option) => option.value === "permissions.remove.session:one",
    )
    app.picker.select.setSelectedIndex(removeIndex)
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "remove_session_permission_rule",
      rule_id: "session:one",
    }))

    const revokeIndex = app.picker.select.options.findIndex(
      (option) => option.value === "permissions.revoke.session:opaque-approval",
    )
    app.picker.select.setSelectedIndex(revokeIndex)
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "revoke_permission_approval",
      approval_id: "session:opaque-approval",
      scope: "session",
    }))
    expect(emitted.some((command) => command.type === "send_message")).toBe(false)
  })

  test("quick-connects fresh built-in providers through connection-scoped auth prompts", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const openedUrls: string[] = []
    const copiedText: string[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        providers: [
          {
            name: "github_copilot",
            authKind: "device_flow",
            nextAction: "configure",
            configured: false,
            authenticated: false,
            reachable: false,
            modelCount: 0,
            status: "setup required",
          },
          {
            name: "openai_codex",
            authKind: "oauth",
            nextAction: "configure",
            configured: false,
            authenticated: false,
            reachable: false,
            modelCount: 0,
            status: "setup required",
          },
        ],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
      externalUrl: {
        async open(url) {
          openedUrls.push(url)
    }
      },
      textClipboard: {
        async writeText(value) {
          copiedText.push(value)
        }
      }
    })
    renderer.root.add(app)

    app.openProviderPicker()
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "configure_builtin_provider",
      provider: "github_copilot",
    }))
    app.handleEvent({
      type: "provider_configured",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "configure-copilot",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      provider: "github_copilot",
      auth_kind: "device_flow",
    })
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "begin_provider_auth",
      provider: "github_copilot",
    }))
    app.handleEvent({
      type: "provider_auth_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "begin-copilot",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      attempt_id: "attempt-1",
      provider: "github_copilot",
      challenge: {
        kind: "device_flow",
        verification_uri: "https://github.com/login/device",
        user_code: "ABCD-1234",
      },
      warnings: [],
    })
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "complete_provider_auth",
      provider: "github_copilot",
      attempt_id: "attempt-1",
    }))
    expect(app.picker.title).toContain("Sign in · GitHub Copilot")
    expect(app.picker.select.options[0]?.description).toContain("ABCD-1234")
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "provider-auth.open",
      "provider-auth.copy-code",
      "provider-auth.copy-url",
      "provider-auth.cancel",
    ])
    app.handleEvent({
      type: "provider_auth_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "begin-copilot-replayed",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "session-local",
      attempt_id: "attempt-1",
      provider: "github_copilot",
      challenge: {
        kind: "device_flow",
        verification_uri: "https://github.com/login/device",
        user_code: "ABCD-1234",
      },
      warnings: [],
    })
    expect(emitted.filter((command) =>
      command.type === "complete_provider_auth" && command.attempt_id === "attempt-1"
    )).toHaveLength(1)
    await Bun.sleep(0)
    expect(openedUrls).toEqual(["https://github.com/login/device"])
    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(openedUrls).toEqual([
      "https://github.com/login/device",
      "https://github.com/login/device",
    ])

    app.picker.select.setSelectedIndex(1)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(copiedText).toEqual(["ABCD-1234"])

    app.picker.select.setSelectedIndex(2)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(copiedText).toEqual(["ABCD-1234", "https://github.com/login/device"])
    expect(app.state.providerAuth.pending?.challenge).toEqual({
      kind: "device_flow",
      verification_uri: "https://github.com/login/device",
      user_code: "ABCD-1234",
    })

    app.handleEvent({
      type: "provider_auth_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "complete-copilot",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      attempt_id: "attempt-1",
      provider: "github_copilot",
      success: true,
      message: "provider authentication completed",
      warnings: [],
    })
    expect(emitted).not.toContainEqual(expect.objectContaining({
      type: "list_models",
      refresh: true,
    }))
    app.handleEvent({
      type: "provider_activation_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "complete-copilot",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "session-local",
      provider: "github_copilot",
      success: true,
      message: "Provider connected. Choose a model from /models.",
    })
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "list_models",
      refresh: true,
    }))

    app.openProviderPicker()
    const codex = app.picker.select.options.findIndex((option) => option.value === "openai_codex")
    app.picker.select.setSelectedIndex(codex)
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "configure_builtin_provider",
      provider: "openai_codex",
    }))
  })

  test("keeps OpenAI API distinct from ChatGPT and shows session workspaces", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        providers: [{
          name: "openai",
          authKind: "oauth",
          nextAction: "authenticate",
          configured: true,
          authenticated: false,
          reachable: false,
          modelCount: 0,
          status: null,
        }],
        sessions: [{
          sessionId: "session-workspace",
          title: "Fix login",
          workspaceName: "payments-service",
          model: "gpt-5",
          driverClientId: null,
          shellActive: false,
        }],
      },
      onCommand: () => ({ type: "accepted" }),
    })
    renderer.root.add(app)

    app.openProviderPicker()
    expect(app.picker.select.options[0]?.name).toBe("OpenAI API")
    expect(app.picker.select.options[0]?.name).not.toContain("ChatGPT")
    expect(app.picker.select.options[0]?.description).not.toContain("ChatGPT")
    app.openSessionPicker()
    expect(app.picker.select.options[0]?.name).toBe("Fix login")
    expect(app.picker.select.options[0]?.description).toContain("payments-service")
  })

  test("offers activation retry and credential replacement for unreachable providers", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const activations: string[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        providers: [{
          name: "openai_codex",
          authKind: "oauth",
          nextAction: "select_models",
          configured: true,
          authenticated: true,
          reachable: false,
          modelCount: 0,
          status: "provider model discovery rejected the stored credential",
        }],
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      async onProviderActivate(provider) {
        activations.push(provider)
      },
    })
    renderer.root.add(app)

    app.openProviderPicker()
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("OpenAI · ChatGPT")
    expect(app.picker.title).not.toContain("openai_codex")
    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "provider-recovery.activate",
      "provider-recovery.reauthenticate",
    ])
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(activations).toEqual(["openai_codex"])

    app.openProviderPicker()
    app.picker.select.selectCurrent()
    const reauthenticate = app.picker.select.options.findIndex(
      (option) => option.value === "provider-recovery.reauthenticate",
    )
    app.picker.select.setSelectedIndex(reauthenticate)
    app.picker.select.selectCurrent()
    expect(commands).toContainEqual(expect.objectContaining({
      type: "begin_provider_auth",
      provider: "openai_codex",
    }))
  })

  test("offers OAuth browser and URL actions with sanitized adapter failures", async () => {
    const setup = await createTestRenderer({
      width: 100,
      height: 24,
      useThread: false,
    })
    renderer = setup.renderer
    const copied: string[] = []
    const authorizationUrl =
      "https://auth.example.test/authorize?state=challenge-canary"
    const app = createRottweilerApp(renderer, {
      onCommand() {
        return { type: "accepted" }
      },
      externalUrl: {
        async open() {
          throw new Error(`launcher leaked ${authorizationUrl}`)
        },
      },
      textClipboard: {
        async writeText(value) {
          copied.push(value)
        },
      },
    })
    renderer.root.add(app)
    app.handleEvent({
      type: "provider_auth_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "begin-codex",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      attempt_id: "attempt-oauth",
      provider: "openai_codex",
      challenge: {
        kind: "oauth",
        authorization_url: authorizationUrl,
        redirect_uri: "http://127.0.0.1:1455/callback",
      },
      warnings: [],
    })

    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "provider-auth.open",
      "provider-auth.copy-url",
      "provider-auth.cancel",
    ])
    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    const error = app.state.errors.at(-1)
    expect(error?.code).toBe("provider_auth_browser_failed")
    expect(error?.message).toContain("Copy URL")
    expect(error?.message).not.toContain("challenge-canary")
    expect(error?.message).not.toContain("launcher leaked")

    const copyUrl = app.picker.select.options.findIndex(
      (option) => option.value === "provider-auth.copy-url",
    )
    app.picker.select.setSelectedIndex(copyUrl)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(copied).toEqual([authorizationUrl])
    expect(
      app.picker.select.options.find((option) => option.value === "provider-auth.open")
        ?.description,
    ).toContain("URL copied")
  })

  test("masks and clears non-protocol provider API keys, including custom providers", async () => {
    const setup = await createTestRenderer({
      width: 100,
      height: 24,
      useThread: false
    })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const submissions: Array<{ provider: string; apiKey: string }> = []
    const app = createRottweilerApp(renderer, {
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      async onProviderApiKey(provider, apiKey) {
        submissions.push({ provider, apiKey })
        return { stored: true, activated: false, warnings: [] }
      }
    })
    renderer.root.add(app)
    const canary = "rw-secret-canary-tui"
    app.openProviderApiKeyPrompt("company-openai")
    await setup.mockInput.typeText(canary)
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain(canary)
    expect(setup.captureCharFrame()).toContain("•".repeat(canary.length))
    expect(JSON.stringify(app.state)).not.toContain(canary)
    expect(JSON.stringify(commands)).not.toContain(canary)

    setup.mockInput.pressEnter()
    await Bun.sleep(10)
    expect(submissions).toEqual([
      { provider: "company-openai", apiKey: canary }
    ])
    expect(app.picker.input.value).toBe("")
    expect(app.state.errors.at(-1)?.code).toBe("provider_activation_pending")
    expect(JSON.stringify(app.state)).not.toContain(canary)
  })

  test("surfaces a correlated rejected model switch as a bounded visible error", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const priorErrors = Array.from({ length: 64 }, (_, index) => ({
      category: "protocol" as const,
      code: `prior-${index}`,
      message: `Prior error ${index}`,
      retryable: false,
    }))
    const app = createRottweilerApp(renderer, {
      requestId: () => "rejected-model-switch",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        errors: priorErrors,
        models: [{
          alias: "fast",
          providers: ["openai"],
          vision: true,
          thinking: true,
          toolCalling: true,
        }],
      },
    })
    renderer.root.add(app)
    app.openModelPicker()
    app.picker.select.selectCurrent()
    app.handleEvent({
      type: "command_acknowledged",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "rejected-model-switch",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      outcome: {
        type: "rejected",
        error: {
          category: "protocol",
          code: "session_not_idle",
          message: "model switching requires an idle session",
          retryable: true,
        },
      },
    })

    expect(commands).toContainEqual(expect.objectContaining({ type: "switch_model", model: "fast" }))
    expect(app.state.errors).toHaveLength(64)
    expect(app.state.errors.at(-1)?.code).toBe("session_not_idle")
    expect(app.banner.visible).toBeTrue()
    expect(app.banner.plainText).toContain("model switching requires an idle session")
    expect(commands).not.toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "project.models.default",
    }))
  })

  test("leaves accepted model persistence to the host transaction", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      requestId: () => `model-correlation-${request++}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
      initialState: {
        ...createInitialState(),
        models: [{
          alias: "fast",
          providers: ["openai"],
          vision: false,
          thinking: true,
          toolCalling: true,
        }],
      },
    })
    renderer.root.add(app)
    for (let index = 0; index < 130; index += 1) {
      app.openModelPicker()
      app.picker.select.selectCurrent()
    }
    const switches = commands.filter((command) => command.type === "switch_model")
    expect(switches).toHaveLength(130)
    const lastRequest = switches.at(-1)?.meta.request_id
    app.handleEvent({
      type: "model_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
        caused_by: lastRequest,
      },
      model: "fast",
    })
    const persisted = commands.filter(
      (command) => command.type === "set_setting" && command.key === "project.models.default",
    )
    expect(persisted).toHaveLength(0)
  })

  test("ignores stale @ search responses by request id", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    let request = 0
    const app = createRottweilerApp(renderer, {
      requestId: () => `workspace-${++request}`,
      onCommand: () => ({ type: "accepted" }),
    })
    renderer.root.add(app)
    app.openFilePicker("old", true)
    app.openFilePicker("new", true)
    const response = (requestId: string, path: string): EngineEvent => ({
      type: "workspace_files_found",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: requestId,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      matches: [{ path, is_directory: false }],
      truncated: false,
    })
    app.handleEvent(response("workspace-1", "old.rs"))
    expect(app.state.workspaceFiles).toEqual([])
    app.handleEvent(response("workspace-2", "new.rs"))
    expect(app.state.workspaceFiles).toEqual([{ path: "new.rs", isDirectory: false }])
  })

  test("attaches and removes a nested workspace file whose path contains spaces", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let request = 0
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      requestId: () => `attachment-${++request}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "compare @first with @screen shot"
    app.composer.editor.cursorOffset = new TextEncoder().encode(app.composer.value).length
    app.openFilePicker("screen shot", true)
    const search = commands.filter((command) => command.type === "search_workspace_files").at(-1)
    expect(search?.type).toBe("search_workspace_files")
    if (search?.type !== "search_workspace_files") throw new Error("missing workspace search")
    app.handleEvent({
      type: "workspace_files_found",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: search.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      matches: [{ path: "docs/UI screen shot.png", is_directory: false }],
      truncated: false,
    })
    app.picker.select.selectCurrent()
    expect(app.composer.value).toBe("compare @first with @screen shot")
    const preview = commands.filter((command) => command.type === "preview_workspace_file").at(-1)
    if (preview?.type !== "preview_workspace_file") throw new Error("missing file preview")
    app.composer.value = `please ${app.composer.value} after lunch`
    app.handleEvent({
      type: "workspace_file_preview_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: preview.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      preview: {
        path: "docs/UI screen shot.png",
        media_type: "image/png",
        data: { type: "inline_base64", data: "iVBORw0KGgo=" },
        total_bytes: "8",
        truncated: false,
      },
    })
    expect(app.composer.attachments).toEqual([{
      name: "UI screen shot.png",
      source_path: "docs/UI screen shot.png",
      media_type: "image/png",
      data: { type: "inline_base64", data: "iVBORw0KGgo=" },
    }])
    expect(app.composer.value).toBe("please compare @first with  after lunch")
    expect(app.composer.removeLastAttachment()).toBeTrue()
    expect(app.composer.attachments).toEqual([])
  })

  test("preserves the exact @ mention when file preview is rejected", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let request = 0
    const app = createRottweilerApp(renderer, {
      requestId: () => `attachment-reject-${++request}`,
      onCommand(command) {
        return command.type === "preview_workspace_file"
          ? {
              type: "rejected",
              error: { category: "protocol", code: "preview", message: "preview unavailable", retryable: true },
            }
          : { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "compare @screen shot with the baseline"
    app.composer.editor.cursorOffset = new TextEncoder().encode("compare @screen shot").length
    app.openFilePicker("screen shot", true)
    app.handleEvent({
      type: "workspace_files_found",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "attachment-reject-1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      matches: [{ path: "docs/UI screen shot.png", is_directory: false }],
      truncated: false,
    })
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.composer.value).toBe("compare @screen shot with the baseline")
    expect(app.composer.attachments).toEqual([])
    expect(app.state.errors.at(-1)?.message).toContain("preview unavailable")
  })

  test("keeps the @ mention when a completed preview cannot fit in the composer", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let request = 0
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      requestId: () => `attachment-full-${++request}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "compare @screen shot"
    app.composer.editor.cursorOffset = new TextEncoder().encode(app.composer.value).length
    app.openFilePicker("screen shot", true)
    const search = commands.find((command) => command.type === "search_workspace_files")
    if (search?.type !== "search_workspace_files") throw new Error("missing workspace search")
    app.handleEvent({
      type: "workspace_files_found",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: search.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      matches: [{ path: "docs/UI screen shot.png", is_directory: false }],
      truncated: false,
    })
    app.picker.select.selectCurrent()
    const preview = commands.find((command) => command.type === "preview_workspace_file")
    if (preview?.type !== "preview_workspace_file") throw new Error("missing file preview")
    for (let index = 0; index < 16; index += 1) {
      app.composer.addAttachment({
        name: `existing-${index}.txt`,
        source_path: `existing/${index}.txt`,
        media_type: "text/plain",
        data: { type: "text", content: String(index) },
      })
    }
    app.handleEvent({
      type: "workspace_file_preview_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: preview.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      preview: {
        path: "docs/UI screen shot.png",
        media_type: "image/png",
        data: { type: "inline_base64", data: "iVBORw0KGgo=" },
        total_bytes: "8",
        truncated: false,
      },
    })
    expect(app.composer.value).toBe("compare @screen shot")
    expect(app.composer.attachments).toHaveLength(16)
    expect(app.state.errors.at(-1)?.message).toContain("at most 16 attachments")
  })

  test("never relocates a delayed preview anchor onto an unrelated matching mention", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let request = 0
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      requestId: () => `stable-anchor-${++request}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "keep @same then @same"
    app.composer.editor.cursorOffset = new TextEncoder().encode(app.composer.value).length
    app.openFilePicker("same", true)
    const search = commands.find((command) => command.type === "search_workspace_files")
    if (search?.type !== "search_workspace_files") throw new Error("missing workspace search")
    app.handleEvent({
      type: "workspace_files_found",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: search.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      matches: [{ path: "docs/same.png", is_directory: false }],
      truncated: false,
    })
    app.picker.select.selectCurrent()
    const preview = commands.find((command) => command.type === "preview_workspace_file")
    if (preview?.type !== "preview_workspace_file") throw new Error("missing file preview")
    app.composer.value = "keep @same then changed"
    app.handleEvent({
      type: "workspace_file_preview_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: preview.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      preview: {
        path: "docs/same.png",
        media_type: "image/png",
        data: { type: "inline_base64", data: "iVBORw0KGgo=" },
        total_bytes: "8",
        truncated: false,
      },
    })
    expect(app.composer.value).toBe("keep @same then changed")
    expect(app.composer.attachments.map((attachment) => attachment.source_path))
      .toEqual(["docs/same.png"])
  })

  test("summarizes long paste as removable context and preserves it until accepted", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    let accept = false
    const app = createRottweilerApp(renderer, {
      imagePaste: { readImage: async () => null, readPath: async () => null },
      onCommand(command) {
        commands.push(command)
        return accept ? { type: "accepted" } : {
          type: "rejected",
          error: { category: "protocol", code: "retry", message: "retry", retryable: true },
        }
      },
    })
    renderer.root.add(app)
    app.composer.focus()
    await setup.mockInput.pasteBracketedText("alpha\nbeta\ngamma")
    expect(app.composer.value).toBe("")
    expect(app.composer.attachments[0]?.name).toBe("Pasted text 1")
    expect(await app.composer.submit()).toBeFalse()
    expect(app.composer.attachments).toHaveLength(1)
    accept = true
    expect(await app.composer.submit()).toBeTrue()
    const sent = commands.filter((command) => command.type === "send_message").at(-1)
    expect(sent?.type === "send_message" ? sent.attachments[0]?.data : null)
      .toEqual({ type: "text", content: "alpha\nbeta\ngamma" })
    expect(app.composer.attachments).toHaveLength(0)
  })

  test("reports an unreadable image path without inserting the local path into the draft", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      imagePaste: {
        readImage: async () => null,
        readPath: async () => { throw new Error("That image path could not be read safely.") },
      },
    })
    renderer.root.add(app)
    app.composer.focus()
    await setup.mockInput.pasteBracketedText("/Users/private/screen shot.png")
    await Bun.sleep(0)
    expect(app.composer.value).toBe("")
    expect(app.composer.attachments).toEqual([])
    expect(app.state.errors.at(-1)?.message).toBe("That image path could not be read safely.")
  })

  test("attaches a clipboard image without intercepting ordinary Ctrl-V text paste", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      imagePaste: {
        readImage: async () => ({
          name: "clipboard.png",
          mediaType: "image/png",
          base64: "iVBORw0KGgo=",
        }),
        readPath: async () => null,
      },
    })
    renderer.root.add(app)
    app.composer.focus()
    setup.mockInput.pressKey("v", { ctrl: true })
    await setup.mockInput.pasteBracketedText("")
    await Bun.sleep(0)
    expect(app.composer.attachments).toEqual([{
      name: "clipboard.png",
      media_type: "image/png",
      data: { type: "inline_base64", data: "iVBORw0KGgo=" },
    }])
  })

  test("accepts the legal two-image envelope and rejects a third image locally", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)
    const image = (fill: number) => ({
      type: "inline_base64" as const,
      data: Buffer.alloc(5 * 1024 * 1024, fill).toString("base64"),
    })
    app.composer.addAttachment({
      name: "one.png",
      media_type: "image/png",
      data: image(1),
    })
    app.composer.addAttachment({
      name: "two.png",
      media_type: "image/png",
      data: image(2),
    })
    app.composer.addAttachment({
      name: "three.png",
      media_type: "image/png",
      data: image(3),
    })
    expect(app.composer.attachments.map((attachment) => attachment.name))
      .toEqual(["one.png", "two.png"])
    expect(app.state.errors.at(-1)?.message).toContain("total at most 10 MiB")
  })

  test("budgets escaped attachment JSON before it can exceed the command transport", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)
    const escapedMiB = "\n".repeat(1024 * 1024)
    for (let index = 0; index < 10; index += 1) {
      app.composer.addAttachment({
        name: `escaped-${index}.txt`,
        source_path: `escaped/${index}.txt`,
        media_type: "text/plain",
        data: { type: "text", content: escapedMiB },
      })
    }
    expect(app.composer.attachments.length).toBeLessThan(10)
    expect(app.state.errors.at(-1)?.message).toContain("too large to send")
  })

  test("keeps a new draft and new attachments while an earlier submission is accepted", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (outcome: CommandOutcome) => void
    const app = createRottweilerApp(renderer, {
      onCommand: () => new Promise<CommandOutcome>((resolve) => { finish = resolve }),
    })
    renderer.root.add(app)
    app.composer.value = "inspect"
    app.composer.addAttachment({
      name: "first.txt",
      media_type: "text/plain",
      data: { type: "text", content: "first" },
    })
    const submission = app.composer.submit()
    app.composer.value = "and then continue"
    app.composer.addAttachment({
      name: "second.txt",
      media_type: "text/plain",
      data: { type: "text", content: "second" },
    })
    finish({ type: "accepted" })
    expect(await submission).toBeTrue()
    expect(app.composer.value).toBe("and then continue")
    expect(app.composer.attachments.map((attachment) => attachment.name)).toEqual(["second.txt"])
  })

  test("restores a rejected in-flight submission without dropping the new draft or attachments", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (outcome: CommandOutcome) => void
    const app = createRottweilerApp(renderer, {
      onCommand: () => new Promise<CommandOutcome>((resolve) => { finish = resolve }),
    })
    renderer.root.add(app)
    app.composer.value = "inspect"
    app.composer.addAttachment({
      name: "first.txt",
      source_path: "src/same.txt",
      media_type: "text/plain",
      data: { type: "text", content: "first" },
    })
    const submission = app.composer.submit()
    app.composer.value = "new draft"
    app.composer.addAttachment({
      name: "second.txt",
      source_path: "src/same.txt",
      media_type: "text/plain",
      data: { type: "text", content: "second" },
    })
    app.composer.addAttachment({
      name: "third.txt",
      source_path: "src/third.txt",
      media_type: "text/plain",
      data: { type: "text", content: "third" },
    })
    finish({
      type: "rejected",
      error: { category: "protocol", code: "retry", message: "retry", retryable: true },
    })
    expect(await submission).toBeFalse()
    expect(app.composer.value).toBe("inspect\nnew draft")
    expect(app.composer.attachments.map((attachment) => attachment.name))
      .toEqual(["second.txt", "third.txt"])
  })

  test("defers retheming until an in-flight rejected submission restores its draft", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (outcome: CommandOutcome) => void
    const app = createRottweilerApp(renderer, {
      theme: systemThemeFor("dark"),
      onCommand: () => new Promise<CommandOutcome>((resolve) => { finish = resolve }),
    })
    renderer.root.add(app)
    app.composer.value = "inspect after theme change"
    app.composer.addAttachment({
      name: "context.txt",
      source_path: "folder with spaces/context.txt",
      media_type: "text/plain",
      data: { type: "text", content: "context" },
    })

    const originalComposer = app.composer
    const submission = app.composer.submit()
    app.setSystemTheme(systemThemeFor("light"))
    expect(app.composer).toBe(originalComposer)
    expect(await app.composer.submit()).toBeFalse()
    finish({
      type: "rejected",
      error: { category: "protocol", code: "retry", message: "retry", retryable: true },
    })

    expect(await submission).toBeFalse()
    await Promise.resolve()
    expect(app.composer).not.toBe(originalComposer)
    expect(app.composer.value).toBe("inspect after theme change")
    expect(app.composer.attachments.map((attachment) => attachment.name)).toEqual(["context.txt"])
  })

  test("cancels a deferred theme preview while a submission is in flight", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (outcome: CommandOutcome) => void
    const app = createRottweilerApp(renderer, {
      theme: kennelTheme,
      onCommand: () => new Promise<CommandOutcome>((resolve) => { finish = resolve }),
    })
    renderer.root.add(app)
    app.composer.value = "keep the original theme"
    const submission = app.composer.submit()
    const originalComposer = app.composer

    app.openThemePicker()
    app.picker.select.setSelectedIndex(
      app.picker.select.options.findIndex((option) => option.value === "theme:tokyonight"),
    )
    app.closePicker()
    expect(app.composer).toBe(originalComposer)
    finish({
      type: "rejected",
      error: { category: "protocol", code: "retry", message: "retry", retryable: true },
    })

    expect(await submission).toBeFalse()
    await Promise.resolve()
    expect(app.composer).not.toBe(originalComposer)
    expect(app.composer.value).toBe("keep the original theme")
    expectCoherentTheme(app, kennelTheme)
  })

  test("replaces a deferred preview when selection returns to the original theme", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (outcome: CommandOutcome) => void
    const app = createRottweilerApp(renderer, {
      theme: kennelTheme,
      onCommand: () => new Promise<CommandOutcome>((resolve) => { finish = resolve }),
    })
    renderer.root.add(app)
    app.composer.value = "keep original preview"
    const submission = app.composer.submit()
    app.openThemePicker()
    app.picker.select.setSelectedIndex(
      app.picker.select.options.findIndex((option) => option.value === "theme:tokyonight"),
    )
    app.picker.select.setSelectedIndex(
      app.picker.select.options.findIndex((option) => option.value === `theme:${kennelTheme.name}`),
    )
    finish({ type: "accepted" })

    expect(await submission).toBeTrue()
    await Promise.resolve()
    expectCoherentTheme(app, kennelTheme)
  })

  test("preserves current attachments before backfilling a rejected full attachment batch", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (outcome: CommandOutcome) => void
    const app = createRottweilerApp(renderer, {
      onCommand: () => new Promise<CommandOutcome>((resolve) => { finish = resolve }),
    })
    renderer.root.add(app)
    app.composer.value = "inspect"
    for (let index = 0; index < 16; index += 1) {
      app.composer.addAttachment({
        name: `old-${index}.txt`,
        source_path: `old/${index}.txt`,
        media_type: "text/plain",
        data: { type: "text", content: String(index) },
      })
    }
    const submission = app.composer.submit()
    app.composer.addAttachment({
      name: "new.txt",
      source_path: "new.txt",
      media_type: "text/plain",
      data: { type: "text", content: "new" },
    })
    finish({
      type: "rejected",
      error: { category: "protocol", code: "retry", message: "retry", retryable: true },
    })
    expect(await submission).toBeFalse()
    expect(app.composer.attachments).toHaveLength(16)
    expect(app.composer.attachments[0]?.name).toBe("new.txt")
    expect(app.state.errors.at(-1)?.message).toContain("rejected send")
  })

  test("keeps only the newest workspace-status and review query responses", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    let request = 0
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      requestId: () => `projection-${++request}`,
      initialState: {
        ...createInitialState(),
        workspaceStatus: {
          workspaceName: "Rottweiler",
          branch: "main",
          changedPaths: ["src/first.rs", "src/second.rs"],
          truncated: false,
        },
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()

    app.handleEvent({
      type: "user_shell_state_changed",
      meta: { ...initialEvent.meta, sequence_id: "status-1" },
      shell_id: "shell-status",
      active: false,
      status: 0,
      captured_output: "",
    })
    app.handleEvent({
      type: "command_finished",
      meta: { ...initialEvent.meta, sequence_id: "status-2" },
      name: "fixture",
      message: "done",
      unrestorable_paths: [],
    })
    const statusRequests = commands.filter((command) => command.type === "get_workspace_status")
    expect(statusRequests).toHaveLength(2)
    const oldStatusRequest = statusRequests[0]!.meta.request_id
    const newStatusRequest = statusRequests[1]!.meta.request_id
    const status = (requestId: string, path: string): EngineEvent => ({
      type: "workspace_status_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: requestId,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      status: { workspace_name: "Rottweiler", branch: "main", changed_paths: [path], truncated: false },
    })
    app.handleEvent(status(oldStatusRequest, "src/stale.rs"))
    expect(app.state.workspaceStatus?.changedPaths).toEqual(["src/first.rs", "src/second.rs"])
    app.handleEvent(status(newStatusRequest, "src/current.rs"))
    expect(app.state.workspaceStatus?.changedPaths).toEqual(["src/current.rs"])

    app.openReview()
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    app.openReview()
    const reviewRequests = commands.filter((command) => command.type === "get_session_review")
    expect(reviewRequests).toHaveLength(2)
    const oldReviewRequest = reviewRequests[0]!.meta.request_id
    const newReviewRequest = reviewRequests[1]!.meta.request_id
    const review = (requestId: string, path: string): EngineEvent => ({
      type: "session_review_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: requestId,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      review: {
        session_id: "session-local",
        files: [{
          path,
          unified_diff: `--- a/${path}\n+++ b/${path}\n-old\n+new\n`,
          status: "pending",
          truncated: false,
          unrestorable_reason: null,
          original_hash: "old",
          current_hash: "new",
        }],
      },
    })
    app.handleEvent(review(oldReviewRequest, "src/first.rs"))
    expect(app.state.review).toBeNull()
    app.handleEvent(review(newReviewRequest, "src/second.rs"))
    expect(app.state.review?.files[0]?.path).toBe("src/second.rs")
    expect(app.reviewPanel.diff.diff).toContain("+new")
  })

  test("keeps context and git status live after a completed turn", async () => {
    const setup = await createTestRenderer({ width: 100, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.handleEvent({
      type: "workspace_status_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-client",
        request_id: "initial-workspace",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      status: {
        workspace_name: "Rottweiler",
        branch: "feature/live-status",
        changed_paths: [],
        truncated: false,
      },
    })
    app.handleEvent({
      type: "context_usage_updated",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "10",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      turn_id: "1",
      used_tokens: "500",
      usable_tokens: "1000",
      reserved_tokens: "100",
      context_window_known: true,
      stable_prefix_hash: "not-presented",
      cache_hit_basis_points: 0,
      estimated_input_tokens: "500",
      provider_input_tokens: "500",
      correction_millionths: "1000000",
    })
    await setup.renderOnce()
    expect(app.statusLine.plainText).toContain("ctx 500/1.0k (50%)")
    expect(app.statusLine.plainText).toContain("git feature/live-status")

    app.handleEvent({
      type: "turn_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "11",
        emitted_at: "2026-01-01T00:00:02Z",
      },
      turn_id: "1",
      status: "completed",
      usage: {
        input_tokens: "500",
        output_tokens: "20",
        cache_read_tokens: "0",
        cache_write_tokens: "0",
        reasoning_tokens: "0",
      },
      cost: { type: "unavailable", reason: "fixture" },
    })
    expect(commands.slice(-3).map((command) => command.type)).toEqual([
      "get_workspace_status",
      "get_context",
      "get_cost",
    ])
  })

  test("preserves picker selection and visible window across unrelated state events", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        commands: Array.from({ length: 20 }, (_, index) => ({
          name: `command-${index}`,
          description: `Command ${index}`,
          usage: `/command-${index}`,
        })),
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    const commandIndex = app.picker.select.options.findIndex(
      (option) => option.value === "slash.command-15",
    )
    expect(commandIndex).toBeGreaterThanOrEqual(0)
    app.picker.select.setSelectedIndex(commandIndex)
    await setup.renderOnce()

    app.handleEvent({
      ...initialEvent,
      meta: { ...initialEvent.meta, sequence_id: "selection-refresh" },
      text: "unrelated state refresh",
    })
    await setup.renderOnce()

    expect(app.picker.select.getSelectedOption()?.value).toBe("slash.command-15")
    expect(app.picker.select.getSelectedIndex()).toBe(commandIndex)
    expect(setup.captureCharFrame()).toContain("Run /command-15")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.picker.visible).toBeFalse()
  })

  test("keeps picker selection readable and distinct in every bundled theme", () => {
    for (const mode of ["dark", "light"] as const) {
      for (const theme of themeCatalogFor(mode)) {
        const selected = pickerSelectionColors(theme)
        expect(colorContrast(selected.foreground, selected.background), theme.name).toBeGreaterThanOrEqual(4.5)
        expect(colorContrast(selected.background, theme.panelRaised), theme.name).toBeGreaterThanOrEqual(1.4)
      }
    }
    const transparentSelection = pickerSelectionColors({
      ...kennelTheme,
      selectedListItemText: "#00000000",
    })
    expect(transparentSelection.foreground).toMatch(/^#[0-9A-Fa-f]{6}$/)
    expect(colorContrast(transparentSelection.foreground, transparentSelection.background)).toBeGreaterThanOrEqual(4.5)
  })

  test("suspends before requesting !python and resumes only on durable inactive", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const ordering: string[] = []
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionId: "session-tui-test",
      terminalHandover: {
        suspend: () => ordering.push("suspend"),
        resume: () => ordering.push("resume"),
      },
      onCommand: (command) => {
        ordering.push("command")
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.composer.value = "!python -q"
    expect(app.composer.shellMode).toBeTrue()
    expect(app.composer.title).toBe(" Shell ")
    expect(app.composer.editor.placeholder).toContain("Shell command")
    expect(await app.composer.submit()).toBeTrue()
    expect(app.composer.shellMode).toBeFalse()
    expect(ordering).toEqual(["suspend", "command"])
    expect(commands).toHaveLength(1)
    expect(commands[0]).toMatchObject({
      type: "user_shell_started",
      session_id: "session-tui-test",
      command: "python -q",
    })

    app.handleEvent({
      type: "user_shell_state_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tui-test",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      shell_id: "shell-1",
      command: "python -q",
      active: true,
    })
    expect(ordering).toEqual(["suspend", "command"])
    await setup.renderOnce()
    expect(app.transcript.mountedCards).toHaveLength(1)
    expect([...app.transcript.mountedCards.values()][0]?.header.plainText).toContain("Shell · running")
    expect(setup.captureCharFrame()).toContain("python -q")

    app.handleEvent({
      type: "user_shell_state_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tui-test",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      shell_id: "shell-1",
      command: "python -q",
      active: false,
      status: 0,
      captured_output: "hello from shell",
    })
    expect(ordering).toEqual(["suspend", "command", "resume", "command"])
    expect(commands.at(-1)).toMatchObject({
      type: "get_workspace_status",
      session_id: "session-tui-test",
    })
    await setup.renderOnce()
    expect(app.transcript.mountedCards).toHaveLength(1)
    expect([...app.transcript.mountedCards.values()][0]?.header.plainText).toContain("exited 0")
    expect(setup.captureCharFrame()).toContain("hello from shell")
  })

  test("preserves a rejected draft and surfaces the protocol error", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionId: "session-tui-test",
      onCommand: () => ({
        type: "rejected",
        error: {
          category: "protocol",
          code: "driver_required",
          message: "take over the driver lease first",
          retryable: false,
        },
      }),
    })
    renderer.root.add(app)
    app.composer.value = "keep this draft"

    expect(await app.composer.submit()).toBeFalse()
    expect(app.composer.value).toBe("keep this draft")
    expect(app.state.errors.at(-1)?.code).toBe("driver_required")
  })

  test("keeps the draft when editor and clipboard integrations are unavailable", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      editor: { compose: async () => null },
      imagePaste: { readImage: async () => null, readPath: async () => null },
    })
    renderer.root.add(app)
    app.composer.value = "draft survives platform failure"

    await app.composer.openExternalEditor()
    expect(await app.composer.pasteImage()).toBeFalse()
    expect(app.composer.value).toBe("draft survives platform failure")
    expect(app.composer.attachments).toHaveLength(0)
  })

  test("routes commands only through the runtime-confirmed session id", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionId: "session-before",
      onCommand: (command) => {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.setSessionId("session-after")
    app.composer.value = "new session only"

    expect(await app.composer.submit()).toBeTrue()
    expect(commands[0]).toMatchObject({
      type: "send_message",
      session_id: "session-after",
    })
  })

  test("routes /review and /fork through typed protocol commands", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionId: "session-actions",
      requestId: () => `request-${commands.length + 1}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.composer.value = "/review"
    expect(await app.composer.submit()).toBeTrue()
    app.composer.value = "/fork "
    expect(await app.composer.submit()).toBeTrue()
    app.handleEvent({
      type: "session_forked",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "fork-client",
        request_id: "request-2",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      parent_session_id: "session-actions",
      child: {
        session_id: "session-actions-first-child",
        workspace_name: "Rottweiler fork",
        model: "fast",
        driver_client_id: null,
        shell_active: false,
      },
      at_turn: "0",
    })
    app.composer.value = "/fork 42"
    expect(await app.composer.submit()).toBeTrue()
    expect(commands.filter((command) => command.type !== "list_commands")).toEqual([
      expect.objectContaining({
        type: "get_session_review",
        session_id: "session-actions",
      }),
      expect.objectContaining({
        type: "fork",
        session_id: "session-actions",
        at_turn: null,
      }),
      expect.objectContaining({
        type: "fork",
        session_id: "session-actions",
        at_turn: "42",
      }),
    ])

    app.composer.value = "/fork not-a-turn extra"
    expect(await app.composer.submit()).toBeFalse()
    expect(commands.filter((command) => command.type !== "list_commands")).toHaveLength(3)
    expect(app.state.errors.at(-1)).toMatchObject({
      code: "invalid_command_arguments",
      message: "usage: /fork [turn] where turn is a decimal u64",
    })
    app.composer.value = "/review extra"
    expect(await app.composer.submit()).toBeFalse()
    expect(commands.filter((command) => command.type !== "list_commands")).toHaveLength(3)
    expect(app.state.errors.at(-1)?.message).toBe("usage: /review")
  })

  test("transitions only from the correlated typed fork result", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const transitions: string[] = []
    const app = createRottweilerApp(renderer, {
      sessionId: "session-parent",
      requestId: () => "fork-request",
      onCommand: () => ({ type: "accepted" }),
      onSessionSelect(sessionId) {
        transitions.push(sessionId)
      },
    })
    renderer.root.add(app)
    app.composer.value = "/fork 42"
    expect(await app.composer.submit()).toBeTrue()
    app.handleEvent({
      type: "session_forked",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "fork-client",
        request_id: "fork-request",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      parent_session_id: "session-parent",
      child: {
        session_id: "session-child",
        workspace_name: "Rottweiler fork",
        model: "fast",
        driver_client_id: null,
        shell_active: false,
      },
      at_turn: "42",
    })

    expect(transitions).toEqual(["session-child"])
    expect(app.state.lastFork).toEqual({
      parentSessionId: "session-parent",
      child: {
        sessionId: "session-child",
        workspaceName: "Rottweiler fork",
        model: "fast",
        driverClientId: null,
        shellActive: false,
      },
      atTurn: "42",
    })
    expect(app.state.lastSequence).toBeNull()

    app.handleEvent({
      type: "session_forked",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "fork-client",
        request_id: "unrelated-fork",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      parent_session_id: "another-parent",
      child: {
        session_id: "wrong-child",
        workspace_name: "Wrong",
        model: "fast",
        driver_client_id: null,
        shell_active: false,
      },
      at_turn: null,
    })
    expect(transitions).toEqual(["session-child"])
  })

  test("clears the fork draft when completion arrives before the POST returns", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const transitions: string[] = []
    let app!: ReturnType<typeof createRottweilerApp>
    app = createRottweilerApp(renderer, {
      sessionId: "fork-parent",
      requestId: () => "fork-race-request",
      async onCommand(command) {
        if (command.type !== "fork") return { type: "accepted" }
        app.handleEvent({
          type: "session_forked",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            client_id: "bound-client",
            request_id: command.meta.request_id,
            emitted_at: "2026-01-01T00:00:00Z",
          },
          parent_session_id: command.session_id,
          child: {
            session_id: "fork-child",
            workspace_name: "workspace",
            model: "fast",
            driver_client_id: "bound-client",
            shell_active: false,
          },
          at_turn: command.at_turn ?? "0",
        })
        await Bun.sleep(0)
        return { type: "accepted" }
      },
      onSessionSelect(sessionId) {
        transitions.push(sessionId)
      },
    })
    renderer.root.add(app)
    app.composer.value = "/fork 4"

    expect(await app.composer.submit()).toBeTrue()
    expect(app.composer.value).toBe("")
    expect(transitions).toEqual(["fork-child"])
  })

  test("blocks review opening and decisions during foreground shell handover", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      initialState: {
        ...createInitialState(),
        shell: { shellId: "shell-active", active: true, status: null, capturedOutput: null },
        review: {
          sessionId: "session-shell-review",
          files: [
            {
              path: "src/lib.rs",
              unifiedDiff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
              status: "pending",
              truncated: false,
              unrestorableReason: null,
              originalHash: "old-state",
              currentHash: "new-state",
            },
          ],
        },
      },
      sessionId: "session-shell-review",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openReview()
    app.composer.value = "/review"
    expect(await app.composer.submit()).toBeFalse()
    setup.mockInput.pressKey("a")

    expect(commands.filter((command) => command.type !== "list_commands")).toEqual([])
    expect(app.state.errors.at(-1)?.code).toBe("review_unavailable_during_shell")
    expect(app.reviewPanel.visible).toBeFalse()
  })

  test("renders historical events in immutable replay presentation", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const historicalEvents: EngineEvent[] = [{
      type: "conversation_turn_committed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-historical",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      agent_turn: "1",
      turn: {
        role: "user",
        blocks: [{ type: "text", text: "Show the saved result." }],
        meta: { synthetic: false, summary: false },
      },
    }, {
      type: "conversation_turn_committed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-historical",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      agent_turn: "1",
      turn: {
        role: "assistant",
        blocks: [{ type: "text", text: "Historical answer rendered through the retained tree." }],
        meta: { synthetic: false, summary: false },
      },
    }]
    const replayedState = historicalEvents.reduce(
      (state, event) => reduceRottweilerState(state, engineEvent(event)),
      createInitialState(),
    )
    const app = createRottweilerApp(renderer, {
      initialState: replayedState,
      replaySessionId: "session-historical",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.handleEvent({
      type: "session_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "replay-client",
        request_id: "replay-request",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "session-historical",
      through_sequence: "2",
    })
    app.composer.value = "must not be sent"

    expect(await app.composer.submit()).toBeFalse()
    await setup.renderOnce()
    expect(app.composer.visible).toBeFalse()
    expect(app.interactionPanel.visible).toBeFalse()
    expect(app.banner.plainText).toContain("Replay · session-historical · read-only")
    expect(app.banner.plainText).toContain("complete through event 2")
    expect(app.transcript.mountedEntryCount).toBe(2)
    expect(app.state.transcript[1]?.turn.blocks).toContainEqual({
      type: "text",
      text: "Historical answer rendered through the retained tree.",
    })
    expect(commands).toEqual([])
  })
})
