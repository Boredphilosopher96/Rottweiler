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

test("image and external editor results belong to the initiating draft generation", async () => {
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const clipboard = deferred<ClipboardImage | null>()
  const editor = deferred<string | null>()
  let scope = "parent"
  let editorCalls = 0
  const composer = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: () => { editorCalls++; return editor.promise } },
    imagePaste: { readImage: () => clipboard.promise, readPath: async () => null },
    onSubmit: () => true, onFileMention: () => {}, submissionScope: () => scope,
  })
  setup.renderer.root.add(composer)
  composer.setImagePasteAvailable(true)
  composer.value = "parent draft"
  const pasted = composer.pasteImage()
  const edited = composer.openExternalEditor()
  await composer.openExternalEditor()
  expect(editorCalls).toBe(1)
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
    imagePaste: { readImage: () => clipboard.promise, readPath: async () => null },
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
    imagePaste: { readImage: async () => null, readPath: () => path.promise },
    onSubmit: () => true, onFileMention: () => {}, onAttachmentError: error => errors.push(error),
  })
  setup.renderer.root.add(composer)
  composer.focus()
  await setup.mockInput.pasteBracketedText("/private/parent.png")
  composer.restoreDraft("replacement draft", [])
  path.reject(new Error("parent path failed"))
  await Bun.sleep(0)
  expect(errors).toEqual([])
  expect(composer.value).toBe("replacement draft")
  const clipboard = deferred<ClipboardImage | null>()
  const retiring = new ComposerRenderable(setup.renderer, kennelTheme, {
    editor: { compose: async () => null },
    imagePaste: { readImage: () => clipboard.promise, readPath: async () => null },
    onSubmit: () => true, onFileMention: () => {},
  })
  retiring.setImagePasteAvailable(true)
  const pending = retiring.pasteImage()
  retiring.destroyRecursively()
  clipboard.resolve(image)
  expect(await pending).toBe(false)
  setup.renderer.destroy()
})
