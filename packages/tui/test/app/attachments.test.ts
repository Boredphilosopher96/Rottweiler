import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, CommandOutcome, EngineEvent } from "../../src/protocol"
import {
  kennelTheme,
  systemThemeFor
} from "../../src/theme"
import { emptySessionReader } from "../fixtures/history"
import { expectCoherentTheme, visionCapableState } from "./fixtures"

describe("Rottweiler attachments", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("ignores stale @ search responses by request id", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
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
      sessionReader: emptySessionReader,
      initialState: visionCapableState(),
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
      sessionReader: emptySessionReader,
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
      sessionReader: emptySessionReader,
      initialState: visionCapableState(),
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
      sessionReader: emptySessionReader,
      initialState: visionCapableState(),
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
      sessionReader: emptySessionReader,
      imagePaste: { readImage: async () => null, preparePath: () => null },
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
      sessionReader: emptySessionReader,
      imagePaste: {
        readImage: async () => null,
        preparePath: () => async () => { throw new Error("That image path could not be read safely.") },
      },
    })
    renderer.root.add(app)
    app.composer.focus()
    app.composer.setImagePasteAvailable(true)
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
      sessionReader: emptySessionReader,
      initialState: visionCapableState(),
      imagePaste: {
        readImage: async () => ({
          name: "clipboard.png",
          mediaType: "image/png",
          base64: "iVBORw0KGgo=",
        }),
        preparePath: () => null,
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

  test("hides and rejects image input for a model without vision", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let reads = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...visionCapableState(),
        model: "openai/text",
        models: [{
          ...visionCapableState().models[0]!,
          id: "openai/text",
          aliases: ["text"],
          vision: false,
        }],
      },
      imagePaste: {
        readImage: async () => {
          reads += 1
          return { name: "hidden.png", mediaType: "image/png", base64: "aW1hZ2U=" }
        },
        preparePath: () => null,
      },
    })
    renderer.root.add(app)

    expect(app.composer.editor.placeholder).not.toContain("image")
    expect(await app.composer.pasteImage()).toBeFalse()
    expect(reads).toBe(0)
    expect(app.composer.addImage({
      name: "hidden.png",
      mediaType: "image/png",
      base64: "aW1hZ2U=",
    })).toBeFalse()
    expect(app.state.errors.at(-1)?.message).toContain("does not support image input")
  })

  test("accepts the legal two-image envelope and rejects a third image locally", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: visionCapableState() })
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
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
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
      sessionReader: emptySessionReader,
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
      sessionReader: emptySessionReader,
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
      .toEqual(["second.txt", "third.txt", "first.txt"])
  })

  test("retains a pending image across a deferred theme rebuild", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (image: { name: string; mediaType: string; base64: string }) => void
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: visionCapableState(),
      theme: systemThemeFor("dark"),
      imagePaste: { readImage: () => new Promise(resolve => { finish = resolve }), preparePath: () => null },
    })
    renderer.root.add(app)
    const original = app.composer
    const input = original.pasteImage()
    original.editor.insertText("typed while reading")
    app.setSystemTheme(systemThemeFor("light"))
    expect(app.composer).toBe(original)
    finish({ name: "pending.png", mediaType: "image/png", base64: "aW1hZ2U=" })
    expect(await input).toBeTrue()
    await Promise.resolve()
    expect(app.composer).not.toBe(original)
    expect(app.composer.value).toBe("typed while reading")
    expect(app.composer.attachments.map(item => item.name)).toEqual(["pending.png"])
  })

  test("defers retheming until an in-flight rejected submission restores its draft", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (outcome: CommandOutcome) => void
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
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
      sessionReader: emptySessionReader,
      theme: kennelTheme,
      onCommand: () => new Promise<CommandOutcome>((resolve) => { finish = resolve }),
    })
    renderer.root.add(app)
    app.composer.value = "keep the original theme"
    const submission = app.composer.submit()
    const originalComposer = app.composer

    app.openThemePicker()
    app.themeBrowser.selectById("theme:tokyonight")
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
      sessionReader: emptySessionReader,
      theme: kennelTheme,
      onCommand: () => new Promise<CommandOutcome>((resolve) => { finish = resolve }),
    })
    renderer.root.add(app)
    app.composer.value = "keep original preview"
    const submission = app.composer.submit()
    app.openThemePicker()
    app.themeBrowser.selectById("theme:tokyonight")
    app.themeBrowser.selectById(`theme:${kennelTheme.name}`)
    finish({ type: "accepted" })

    expect(await submission).toBeTrue()
    await Promise.resolve()
    expectCoherentTheme(app, kennelTheme)
  })

  test("retains every rejected attachment and refuses resubmission until the combined draft fits", async () => {
    const setup = await createTestRenderer({ width: 88, height: 20, useThread: false })
    renderer = setup.renderer
    let finish!: (outcome: CommandOutcome) => void
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
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
    expect(app.composer.attachments).toHaveLength(17)
    expect(app.composer.attachments[0]?.name).toBe("new.txt")
    expect(app.composer.attachments.at(-1)?.name).toBe("old-15.txt")
    expect(await app.composer.submit()).toBe(false)
    expect(app.composer.attachments).toHaveLength(17)
    expect(app.state.errors.at(-1)?.message).toContain("too large to send")
  })
})
