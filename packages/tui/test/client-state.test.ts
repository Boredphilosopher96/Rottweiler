import { createStreamingTail } from "../src/state/model"
import { toolOutputBuffer } from "../src/state/display-buffer"
import { afterEach, describe, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../src/app"
import { mkdtempSync, rmSync } from "node:fs"
import { join } from "node:path"
import { tmpdir } from "node:os"
import { createInitialState, type ToolProjection } from "../src/state"
import { parseTuiRecycleState, MAX_RECYCLE_STATE_BYTES, recycleTuiIfNeeded } from "../src/recycle-state"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

describe("client-owned renderer handoff", () => {
  test("capture, destroy, recreate and restore retains attachments, editing selection and active palette", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const original = createRottweilerApp(renderer, { onCommand: () => null })
    renderer.root.add(original)
    original.composer.value = "unfinished draft"
    original.composer.addAttachment({ name: "notes.txt", source_path: "notes.txt", media_type: "text/plain", data: { type: "text", content: "keep these notes" } })
    original.composer.addAttachment({ name: "patch.diff", media_type: "text/plain", data: { type: "text", content: "+ keep this diff" } })
    original.composer.editor.cursorOffset = 7
    original.composer.editor.setSelection(1, 5)
    original.showToolsView()
    original.openCommandPicker()
    original.commandPalette.input.value = "model"
    await setup.renderOnce()
    original.commandPalette.moveSelection(1)
    await setup.renderOnce()
    const saved = original.recycleState()
    if (saved === null) throw new Error("expected restorable client state")
    expect(saved.picker?.kind).toBe("palette")
    expect(saved.picker?.selectedId).not.toBeNull()
    expect(saved.composer.attachments).toHaveLength(2)
    const replayed = original.state
    renderer.root.remove(original)
    original.destroyRecursively()

    const replacement = createRottweilerApp(renderer, { initialState: replayed, onCommand: () => null })
    renderer.root.add(replacement)
    const serialized = parseTuiRecycleState(JSON.parse(JSON.stringify(saved)))
    if (serialized === null) throw new Error("handoff must round trip")
    replacement.restoreRecycleState(serialized)
    await setup.renderOnce()
    replacement.applyPendingRecycleScroll()
    expect(replacement.composer.value).toBe("unfinished draft")
    expect(replacement.composer.attachments).toEqual(saved.composer.attachments)
    expect(replacement.composer.editor.cursorOffset).toBe(saved.composer.cursorOffset)
    expect(replacement.composer.editor.getSelection()).toEqual(saved.composer.selection)
    expect(replacement.primaryView).toBe("tools")
    expect(replacement.commandPalette.visible).toBe(true)
    expect(replacement.commandPalette.input.value).toBe(saved.picker?.query ?? "")
    expect(replacement.commandPalette.selectedId).toBe(saved.picker?.selectedId ?? null)
    expect(replacement.commandPalette.input.focused).toBe(true)
    replacement.closePicker()
    expect(replacement.composer.editor.focused).toBe(true)
  })

  test("retains transcript and Tools folds and selected blocks across replacement", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const tool: ToolProjection = {
      toolCallId: "one", turnId: "turn", name: "read", args: { path: "one.txt" }, status: "running",
      capabilities: ["read_filesystem"], rationale: null, diff: null, chunks: toolOutputBuffer([{ stream: "stdout", chunk: "some output" }]),
      output: null, isError: null, callIndex: 0, timing: { kind: "unknown" },
    }
    const initial = { ...createInitialState(), tools: { one: tool }, streamingTail: createStreamingTail({
      turnId: "turn", text: "", thinking: "reasoning", citations: [], toolCallIds: ["one"], finished: null,
    }) }
    const original = createRottweilerApp(renderer, { initialState: initial })
    renderer.root.add(original)
    await setup.renderOnce()
    original.transcript.selectNextBlock()
    original.transcript.toggleSelectedBlock()
    original.transcript.selectNextBlock()
    original.transcript.toggleSelectedBlock()
    original.showToolsView()
    original.toolsWorkspace.selectNextBlock()
    original.toolsWorkspace.toggleSelectedBlock()
    original.showConversationView()
    const saved = original.recycleState()
    if (saved === null) throw new Error("expected restorable block state")
    expect(saved.transcript.blocks.selectedId).not.toBeNull()
    expect(saved.tools.selectedId).not.toBeNull()
    renderer.root.remove(original)
    original.destroyRecursively()
    const replacement = createRottweilerApp(renderer, { initialState: initial })
    renderer.root.add(replacement)
    replacement.restoreRecycleState(saved)
    await setup.renderOnce()
    replacement.applyPendingRecycleScroll()
    expect(replacement.transcript.captureClientState()).toEqual(saved.transcript)
    expect(replacement.primaryView).toBe("conversation")
    expect(replacement.toolsWorkspace.captureClientState()).toEqual(saved.tools)
  })

  test("the production memory-check path never recycles an active review or a failed handoff", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { onCommand: () => null })
    renderer.root.add(app)
    app.openReview()
    const root = mkdtempSync(join(tmpdir(), "rw-client-recycle-"))
    let exits = 0
    try {
      expect(recycleTuiIfNeeded({ observedBytes: 500 * 1024 * 1024, thresholdBytes: 384 * 1024 * 1024,
        path: join(root, "handoff.json"), capture: () => app.recycleState(), recycle: () => { exits += 1 },
      })).toBe(false)
      expect(exits).toBe(0)
      expect(app.reviewPanel.visible).toBe(true)
      expect(app.isDestroyed).toBe(false)
      renderer.root.remove(app)
      app.destroyRecursively()
      const plain = createRottweilerApp(renderer)
      expect(recycleTuiIfNeeded({ observedBytes: 500 * 1024 * 1024, thresholdBytes: 384 * 1024 * 1024,
        path: undefined, capture: () => plain.recycleState(), recycle: () => { exits += 1 },
      })).toBe(false)
      expect(exits).toBe(0)
      expect(recycleTuiIfNeeded({ observedBytes: 500 * 1024 * 1024, thresholdBytes: 384 * 1024 * 1024,
        path: join(root, "handoff.json"), capture: () => plain.recycleState(), recycle: () => { exits += 1 },
      })).toBe(true)
      expect(exits).toBe(1)
      plain.destroyRecursively()
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })

  test("restores a detached child draft only in its owning session", async () => {
    const setup = await createTestRenderer({ width: 90, height: 25, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)
    const state = app.recycleState()
    if (state === null) throw new Error("expected restorable state")
    const children = [{ id: "child-one", draft: { content: "child draft", attachments: [{ name: "child.txt", media_type: "text/plain", data: { type: "text" as const, content: "child attachment" } }] } }]
    app.restoreRecycleState({ ...state, subagentDrafts: children })
    expect(app.recycleState()?.subagentDrafts).toEqual(children)
    app.restoreRecycleState({ ...state, sessionId: "some-other-session", composer: { ...state.composer, content: "wrong draft" } })
    expect(app.composer.value).toBe(state.composer.content)
  })

  test("defers secret/callback interactions and oversized drafts instead of losing them", async () => {
    const setup = await createTestRenderer({ width: 90, height: 25, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)
    app.openProviderApiKeyPrompt("openai")
    app.picker.input.value = "sk-never-write-this"
    expect(app.recycleState()).toBeNull()
    expect(app.picker.input.focused).toBe(true)
    app.closePicker()
    app.composer.value = "large attachment draft"
    for (let index = 0; index < 8; index += 1) {
      expect(app.composer.addAttachment({ name: `large-${index}.txt`, media_type: "text/plain", data: {
        type: "text", content: String(index) + "x".repeat(MAX_RECYCLE_STATE_BYTES / 8 - 1),
      } })).toBe(true)
    }
    expect(app.recycleState() === null).toBe(true)
    expect(app.composer.attachments).toHaveLength(8)
    expect(app.composer.value).toBe("large attachment draft")
  })
})
