import { ClientAllocationOwner } from "../src/client-allocation"
import { expect, test } from "bun:test"
import { ComposerDraftStore, composerDraftBytes, MAX_COMPOSER_TEXT_BYTES } from "../src/composer-drafts"
import type { ComposerDraft } from "../src/subagent-state"

const draft = (content: string): ComposerDraft => ({ content, attachments: [] })
const attachment = { name: "a.txt", media_type: "text/plain", data: { type: "text" as const, content: "body" } }

test("active, suspended and pending drafts compete for one budget without eviction", () => {
  const owner = new ComposerDraftStore(2400, 4)
  expect(owner.set("parent", draft("p".repeat(100)))).toBe(true)
  expect(owner.set("child:a", draft("a".repeat(100)))).toBe(true)
  expect(owner.set("child:b", draft("b".repeat(100)))).toBe(false)
  const before = owner.usage.bytes
  const pending = owner.submit("parent")!
  expect(owner.usage.bytes).toBe(before)
  expect(owner.usage.pending).toBe(1)
  expect(owner.set("parent", draft("new"))).toBe(true)
  expect(owner.set("child:b", draft("b".repeat(100)))).toBe(false)
  const restored = pending.settle(false)
  expect(restored?.content).toBe(`${"p".repeat(100)}\nnew`)
  expect(owner.get("child:a").content).toBe("a".repeat(100))
  expect(owner.usage.bytes).toBeLessThanOrEqual(2400)
  expect(pending.settle(false)).toBeNull()
  expect(() => pending.draft).toThrow("settled")
})

test("a rejected submission preserves exact attachment versions and deduplicates identical values", () => {
  const owner = new ComposerDraftStore()
  owner.set("child:a", { content: "submitted", attachments: [attachment] })
  const pending = owner.submit("child:a")!
  owner.set("child:a", { content: "new", attachments: [attachment, { ...attachment, data: { type: "text", content: "changed body" } }] })
  const restored = pending.settle(false)!
  expect(restored.content).toBe("submitted\nnew")
  expect(restored.attachments.map(item => item.data)).toEqual([{ type: "text", content: "body" }, { type: "text", content: "changed body" }])
  expect(owner.usage.bytes).toBe(composerDraftBytes(restored) + "child:a".length * 2)
})

test("reset retires pending destinations while retaining their allocation until settlement", () => {
  const owner = new ComposerDraftStore()
  owner.set("parent", draft("old session"))
  const pending = owner.submit("parent")!
  const bytes = owner.usage.bytes
  owner.clear()
  expect(owner.usage.bytes).toBe(bytes)
  owner.set("parent", draft("new session"))
  expect(pending.settle(false)).toBeNull()
  expect(owner.get("parent").content).toBe("new session")
  expect(owner.usage.bytes).toBe(composerDraftBytes(owner.get("parent")) + 12)
})

test("handoff admission is atomic and caller mutation cannot change charged attachment data", () => {
  const owner = new ComposerDraftStore(2400, 2)
  const data = { ...attachment, data: { type: "text" as const, content: "body" } }
  owner.set("parent", { content: "original", attachments: [data] })
  data.data.content = "caller mutation"
  expect(owner.get("parent").attachments[0]?.data).toEqual({ type: "text", content: "body" })
  expect(owner.replace([{ scope: "parent", draft: draft("x".repeat(400)) }])).toBe(false)
  expect(owner.get("parent").content).toBe("original")
  owner.remove("parent")
  expect(owner.usage.bytes).toBe(0)
})

test("native insertion refuses before allocating an over-budget draft, including suspended data", async () => {
  const { createTestRenderer } = await import("@opentui/core/testing")
  const { ComposerRenderable } = await import("../src/components/composer")
  const { kennelTheme } = await import("../src/theme")
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const owner = new ComposerDraftStore(2400, 4)
  owner.set("child", draft("s".repeat(100)))
  const errors: string[] = []
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    drafts: owner, editor: { compose: async () => null },
    imagePaste: { readImage: async () => null, preparePath: () => null },
    onSubmit: () => true, onFileMention: () => {}, onAttachmentError: error => errors.push(error),
  })
  setup.renderer.root.add(composer)
  composer.value = "current draft"
  composer.editor.insertText("x".repeat(100_000))
  expect(composer.value).toBe("current draft")
  expect(errors).toHaveLength(1)
  expect(owner.get("child").content).toBe("s".repeat(100))
  composer.editor.setSelection(0, 7)
  composer.editor.insertText("updated")
  await setup.flush()
  expect(composer.value).toBe("updated draft")
  expect(owner.get("default").content).toBe("updated draft")
  expect(owner.usage.bytes).toBeLessThanOrEqual(2400)
  setup.renderer.destroy()
})

test("native edit checkpoints release history without changing text, cursor or selection", async () => {
  const { createTestRenderer } = await import("@opentui/core/testing")
  const { ComposerEditorRenderable } = await import("../src/components/composer-editor")
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const editor = new ComposerEditorRenderable(setup.renderer, { id: "bounded-editor" }, () => true, new ClientAllocationOwner(), 2048)
  setup.renderer.root.add(editor)
  editor.insertText("start")
  expect(editor.editBuffer.canUndo()).toBe(true)
  for (let index = 0; index < 100; index++) {
    editor.insertText(" next")
    expect(editor.historyCharge).toBeLessThanOrEqual(2048)
  }
  expect(editor.plainText).toBe("start" + " next".repeat(100))
  expect(editor.cursorOffset).toBe(editor.plainText.length)
  expect(editor.editBuffer.canUndo()).toBe(true)
  editor.editBuffer.clearHistory()
  expect(editor.editBuffer.canUndo()).toBe(false)
  editor.setSelection(1, 3)
  editor.insertText("XY")
  expect(editor.plainText.startsWith("sXYrt")).toBe(true)
  expect(editor.cursorOffset).toBe(3)
  setup.renderer.destroy()
})

test("destroying a composer does not release its pending submission allocation early or restore into dead native state", async () => {
  const { createTestRenderer } = await import("@opentui/core/testing")
  const { ComposerRenderable } = await import("../src/components/composer")
  const { kennelTheme } = await import("../src/theme")
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const owner = new ComposerDraftStore()
  let finish!: (accepted: boolean) => void
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    drafts: owner, editor: { compose: async () => null },
    imagePaste: { readImage: async () => null, preparePath: () => null },
    onSubmit: () => new Promise<boolean>(resolve => { finish = resolve }), onFileMention: () => {},
  })
  setup.renderer.root.add(composer)
  composer.value = "retained until reply"
  const pending = composer.submit()
  const bytes = owner.usage.bytes
  owner.clear()
  composer.destroyRecursively()
  expect(owner.usage.bytes).toBe(bytes)
  finish(false)
  expect(await pending).toBe(false)
  expect(owner.usage.bytes).toBe(0)
  setup.renderer.destroy()
})

test("closing a child retires its rollback destination but holds pending bytes until the reply", () => {
  const owner = new ComposerDraftStore()
  owner.set("child:a", draft("submitted"))
  const pending = owner.submit("child:a")!
  owner.remove("child:a")
  expect(owner.usage.bytes).toBeGreaterThan(0)
  expect(pending.settle(false)).toBeNull()
  expect(owner.get("child:a").content).toBe("")
  expect(owner.usage.bytes).toBe(0)
})

test("input read reservations retain capacity across clear and reserve attachment slots", () => {
  const owner = new ComposerDraftStore(4096, 8)
  owner.set("parent", draft("typed"))
  const read = owner.reserveDraft("parent", 2048, 1)!
  expect(read).not.toBeNull()
  expect(owner.reserveDraft("child", 100, 0)).toBeNull()
  expect(owner.submit("parent")).toBeNull()
  expect(owner.set("parent", { content: "too many", attachments: Array.from({ length: 40 }, () => attachment) })).toBe(false)
  expect(owner.set("parent", draft("new text"))).toBe(true)
  const restored = read.finish({ content: "", attachments: [attachment] }).settle(false)!
  expect(restored.content).toBe("new text")
  expect(restored.attachments).toEqual([attachment])
  expect(owner.usage.pending).toBe(0)
  const pending = owner.reserveDraft("parent", 2000, 0)!
  owner.clear()
  expect(owner.usage.bytes).toBe(2012)
  expect(owner.reserveDraft("parent", 500, 0)).toBeNull()
  pending.cancel()
  expect(owner.usage.bytes).toBe(0)
})


test("native editing admits the complete UTF-8 limit and refuses overflow without changing selection", async () => {
  const { createTestRenderer } = await import("@opentui/core/testing")
  const { ComposerEditorRenderable } = await import("../src/components/composer-editor")
  const setup = await createTestRenderer({ width: 40, height: 10, useThread: false })
  const editor = new ComposerEditorRenderable(setup.renderer, { id: "large-editor" }, () => true, new ClientAllocationOwner())
  setup.renderer.root.add(editor)
  try {
    const text = "é".repeat(MAX_COMPOSER_TEXT_BYTES / 2)
    editor.setText(text)
    expect(editor.plainText).toBe(text)
    editor.cursorOffset = text.length
    editor.insertText("x")
    expect(editor.plainText).toBe(text)
    editor.setSelection(0, text.length)
    editor.setText(text + "x")
    expect(editor.getSelectedText()).toBe(text)
    editor.insertText("replacement")
    expect(editor.plainText).toBe("replacement")
  } finally { setup.renderer.destroy() }
})


test("pending submitted text reserves the exact UTF-8 rollback destination", () => {
  const owner = new ComposerDraftStore()
  const text = "é".repeat(MAX_COMPOSER_TEXT_BYTES / 2 - 2)
  expect(owner.set("parent", draft(text))).toBe(true)
  const pending = owner.submit("parent")!
  expect(owner.set("parent", draft("xy"))).toBe(true)
  expect(owner.set("parent", draft("overflow"))).toBe(false)
  expect(pending.settle(false)?.content).toBe(text + "\nxy")
  expect(owner.get("parent").content).toBe(text + "\nxy")
  owner.clear()
})

test("ordinary keys on a near-limit draft retain delta undo instead of resetting the entire buffer", async () => {
  const { createTestRenderer } = await import("@opentui/core/testing")
  const { ComposerEditorRenderable } = await import("../src/components/composer-editor")
  const setup = await createTestRenderer({ width: 40, height: 10, useThread: false })
  const editor = new ComposerEditorRenderable(setup.renderer, { id: "delta-editor" }, () => true, new ClientAllocationOwner())
  setup.renderer.root.add(editor)
  try {
    const text = "x".repeat(MAX_COMPOSER_TEXT_BYTES - 64)
    editor.setText(text); editor.cursorOffset = text.length
    for (let index = 0; index < 32; index++) editor.insertChar("y")
    expect(editor.plainText).toBe(text + "y".repeat(32))
    expect(editor.historyCharge).toBeLessThan(32 * 1024)
    expect(editor.editBuffer.canUndo()).toBe(true)
    editor.editBuffer.undo()
    // Native direct mutation is followed by its native content event before model observation.
    await setup.renderOnce()
    expect(editor.plainText).toBe(text + "y".repeat(31))
  } finally { setup.renderer.destroy() }
})


test("composer set, restore and external editor refusal preserve the prior draft and explain attachment input", async () => {
  const { createTestRenderer } = await import("@opentui/core/testing")
  const { ComposerRenderable } = await import("../src/components/composer")
  const { kennelTheme } = await import("../src/theme")
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const errors: string[] = []
  const tooLarge = "x".repeat(MAX_COMPOSER_TEXT_BYTES + 1)
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: async () => tooLarge }, imagePaste: { readImage: async () => null, preparePath: () => null },
    onSubmit: () => true, onFileMention: () => {}, onAttachmentError: message => errors.push(message),
  })
  setup.renderer.root.add(composer)
  try {
    composer.restoreDraft("keep", [attachment])
    composer.value = tooLarge
    composer.restoreDraft(tooLarge, [])
    await composer.openExternalEditor()
    expect(composer.value).toBe("keep")
    expect(composer.attachments).toEqual([attachment])
    expect(errors).toHaveLength(3)
    expect(errors.every(message => message.includes("Attach large content as a file"))).toBe(true)
  } finally { setup.renderer.destroy() }
})
