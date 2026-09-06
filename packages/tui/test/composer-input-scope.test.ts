import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { ComposerRenderable } from "../src/components/composer"
import type { ClipboardImage } from "../src/platform"
import { kennelTheme } from "../src/theme"

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (error: unknown) => void
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const image: ClipboardImage = { name: "parent.png", mediaType: "image/png", base64: "iVBORw0KGgo=" }

test("one admitted image read excludes another input and belongs to its initiating draft generation", async () => {
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const clipboard = deferred<ClipboardImage | null>()
  const editor = deferred<string | null>()
  let scope = "parent"
  let editorCalls = 0
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: () => { editorCalls++; return editor.promise } },
    imagePaste: { readImage: () => clipboard.promise, preparePath: () => null },
    onSubmit: () => true, onFileMention: () => {}, submissionScope: () => scope,
  })
  setup.renderer.root.add(composer)
  composer.setImagePasteAvailable(true)
  composer.value = "parent draft"
  const pasted = composer.pasteImage()
  const edited = composer.openExternalEditor()
  await composer.openExternalEditor()
  expect(editorCalls).toBe(0)
  scope = "child"
  composer.restoreDraft("child draft", [])
  clipboard.resolve(image)
  editor.resolve("edited parent draft")
  await edited
  expect(await pasted).toBe(false)
  expect(composer.value).toBe("child draft")
  expect(composer.attachments).toEqual([])
  setup.renderer.destroy()
})

test("returning to the same scope does not revive a result from an earlier draft", async () => {
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const clipboard = deferred<ClipboardImage | null>()
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: async () => null },
    imagePaste: { readImage: () => clipboard.promise, preparePath: () => null },
    onSubmit: () => true, onFileMention: () => {}, submissionScope: () => "parent",
  })
  setup.renderer.root.add(composer)
  composer.setImagePasteAvailable(true)
  const pasted = composer.pasteImage()
  composer.restoreDraft("new parent generation", [])
  clipboard.resolve(image)
  expect(await pasted).toBe(false)
  expect(composer.attachments).toEqual([])
  setup.renderer.destroy()
})

test("late pasted-path failures and results cannot affect a replaced or destroyed composer", async () => {
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const path = deferred<ClipboardImage | null>()
  const errors: string[] = []
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: async () => null },
    imagePaste: { readImage: async () => null, preparePath: () => () => path.promise },
    onSubmit: () => true, onFileMention: () => {}, onAttachmentError: error => errors.push(error),
  })
  setup.renderer.root.add(composer)
  composer.focus()
  composer.setImagePasteAvailable(true)
  await setup.mockInput.pasteBracketedText("/private/parent.png")
  composer.restoreDraft("replacement draft", [])
  path.reject(new Error("parent path failed"))
  await Bun.sleep(0)
  expect(errors).toEqual([])
  expect(composer.value).toBe("replacement draft")
  const clipboard = deferred<ClipboardImage | null>()
  const retiring = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: async () => null },
    imagePaste: { readImage: () => clipboard.promise, preparePath: () => null },
    onSubmit: () => true, onFileMention: () => {},
  })
  retiring.setImagePasteAvailable(true)
  const pending = retiring.pasteImage()
  retiring.destroyRecursively()
  clipboard.resolve(image)
  expect(await pending).toBe(false)
  setup.renderer.destroy()
})

test("plain text paste is synchronous at its selection and image admission does not fan out", async () => {
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const clipboard = deferred<ClipboardImage | null>()
  let reads = 0
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: async () => null },
    imagePaste: { readImage: () => { reads++; return clipboard.promise }, preparePath: () => null },
    onSubmit: () => true, onFileMention: () => {},
  })
  setup.renderer.root.add(composer)
  composer.value = "before after"
  composer.editor.cursorOffset = 7
  composer.editor.setSelection(7, 12)
  composer.focus()
  await setup.mockInput.pasteBracketedText("replacement")
  composer.editor.insertText("!")
  expect(composer.value).toBe("before replacement!")
  composer.setImagePasteAvailable(true)
  const pending = composer.pasteImage()
  expect(await composer.pasteImage()).toBe(false)
  expect(reads).toBe(1)
  composer.editor.cursorOffset = 3
  clipboard.resolve(image)
  expect(await pending).toBe(true)
  expect(composer.editor.cursorOffset).toBe(3)
  expect(composer.value).toBe("before replacement!")
  expect(composer.attachments).toHaveLength(1)
  setup.renderer.destroy()
})

test("external editor preserves same-generation concurrent input and ignores replaced scopes", async () => {
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  let editor = deferred<string | null>()
  let calls = 0
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: () => { calls++; return editor.promise } },
    imagePaste: { readImage: async () => null, preparePath: () => null },
    onSubmit: () => true, onFileMention: () => {},
  })
  setup.renderer.root.add(composer)
  composer.value = "initial"
  const pending = composer.openExternalEditor()
  await composer.openExternalEditor()
  expect(calls).toBe(1)
  composer.editor.cursorOffset = 7
  composer.editor.insertText(" concurrent")
  editor.resolve("external result")
  await pending
  expect(composer.value).toBe("external result\ninitial concurrent")
  editor = deferred<string | null>()
  const stale = composer.openExternalEditor()
  composer.restoreDraft("replacement", [])
  editor.resolve("stale")
  await stale
  expect(composer.value).toBe("replacement")
  setup.renderer.destroy()
})

test("external editor completion cannot take focus from a newer interaction", async () => {
  const { TextareaRenderable } = await import("@opentui/core")
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const editor = deferred<string | null>()
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: () => editor.promise },
    imagePaste: { readImage: async () => null, preparePath: () => null },
    onSubmit: () => true, onFileMention: () => {},
  })
  setup.renderer.root.add(composer)
  const overlay = new TextareaRenderable(setup.renderer, { id: "new-interaction" })
  setup.renderer.root.add(overlay)
  composer.value = "draft"
  composer.focus()
  const pending = composer.openExternalEditor()
  overlay.focus()
  editor.resolve("edited draft")
  await pending
  expect(overlay.focused).toBe(true)
  expect(composer.editor.focused).toBe(false)
  expect(composer.value).toBe("edited draft")
  setup.renderer.destroy()
})
