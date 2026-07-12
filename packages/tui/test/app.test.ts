import { afterEach, describe, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import type { ClientCommand, EngineEvent } from "../src/protocol"
import { PROTOCOL_VERSION } from "../../../protocol/types"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"

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

describe("Rottweiler OpenTUI shell", () => {
  let renderer: TestRenderer | undefined

  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
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
    expect(frame).toContain("model fast")

    const cells = setup.captureSpans()
    expect(cells.cols).toBe(72)
    expect(cells.rows).toBe(12)
    expect(cells.lines).toHaveLength(12)
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
    })
    renderer.root.add(app)
    await setup.mockInput.typeText("/")
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.select.getSelectedIndex()).toBe(0)

    setup.mockInput.pressKey("p", { ctrl: true })
    expect(app.picker.select.getSelectedIndex()).toBe(commands.length + 1)
    setup.mockInput.pressKey("n", { ctrl: true })
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    setup.mockInput.pressKey("\x1b[6~")
    expect(app.picker.select.getSelectedIndex()).toBe(10)
    setup.mockInput.pressKey("\x1b[5~")
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    setup.mockInput.pressKey("END")
    expect(app.picker.select.getSelectedIndex()).toBe(commands.length + 1)
    setup.mockInput.pressKey("HOME")
    expect(app.picker.select.getSelectedIndex()).toBe(0)
    setup.mockInput.pressArrow("up")
    expect(app.picker.select.getSelectedIndex()).toBe(commands.length + 1)
    setup.mockInput.pressArrow("down")
    expect(app.picker.select.getSelectedIndex()).toBe(0)

    setup.mockInput.pressEnter()
    expect(app.picker.visible).toBeFalse()
    expect(app.composer.value).toBe("/command-0 ")
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
    await setup.mockInput.typeText("/mod")
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
    expect(app.picker.visible).toBeFalse()
    expect(app.composer.value).toBe("/command-1 ")
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
    const visible = Math.max(1, Math.floor(app.picker.select.height / 2))
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
    app.picker.select.setSelectedIndex(15)
    await setup.renderOnce()

    app.handleEvent({
      ...initialEvent,
      meta: { ...initialEvent.meta, sequence_id: "selection-refresh" },
      text: "unrelated state refresh",
    })
    await setup.renderOnce()

    expect(app.picker.select.getSelectedOption()?.value).toBe("command-15")
    expect(app.picker.select.getSelectedIndex()).toBe(15)
    expect(setup.captureCharFrame()).toContain("/command-15")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.picker.visible).toBeFalse()
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
    expect(await app.composer.submit()).toBeTrue()
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
      captured_output: "",
    })
    expect(ordering).toEqual(["suspend", "command", "resume", "command"])
    expect(commands.at(-1)).toMatchObject({
      type: "get_workspace_status",
      session_id: "session-tui-test",
    })
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
      imagePaste: { readImage: async () => null },
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
