import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import {
  createDesktopNotificationAdapter,
  createExternalEditorAdapter,
  createExternalUrlAdapter,
  createImagePasteAdapter,
  createTextClipboardAdapter,
  parseCommandLine,
  type ProcessExecutionOptions,
  type ProcessExecutionResult,
  type ProcessExecutor,
} from "../src/platform"

const PNG = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1])

class RecordingExecutor implements ProcessExecutor {
  readonly calls: Array<{
    executable: string
    args: readonly string[]
    options: ProcessExecutionOptions | undefined
  }> = []
  handler: (
    executable: string,
    args: readonly string[],
    options: ProcessExecutionOptions | undefined,
  ) => Promise<ProcessExecutionResult> = async () => ({ status: 0, stdout: new Uint8Array() })

  async run(
    executable: string,
    args: readonly string[],
    options?: ProcessExecutionOptions,
  ): Promise<ProcessExecutionResult> {
    this.calls.push({ executable, args, options })
    return await this.handler(executable, args, options)
  }
}

describe("production TUI platform adapters", () => {
  let temporaryDirectory: string | null = null

  afterEach(async () => {
    if (temporaryDirectory !== null) {
      await rm(temporaryDirectory, { recursive: true, force: true })
      temporaryDirectory = null
    }
  })

  test("runs $EDITOR without a shell, suspends the renderer, and reads the private buffer", async () => {
    temporaryDirectory = await mkdtemp(join(tmpdir(), "rw-platform-test-"))
    const ordering: string[] = []
    const executor = new RecordingExecutor()
    executor.handler = async (_executable, args) => {
      ordering.push("editor")
      const path = args.at(-1)
      if (path === undefined) throw new Error("missing prompt path")
      expect(await readFile(path, "utf8")).toBe("original draft")
      await writeFile(path, "edited draft\n", "utf8")
      return { status: 0, stdout: new Uint8Array() }
    }
    const adapter = createExternalEditorAdapter(
      {
        suspend: () => ordering.push("suspend"),
        resume: () => ordering.push("resume"),
      },
      {
        environment: { EDITOR: 'mock-editor --wait "profile one"; touch /tmp/nope' },
        executor,
        temporaryRoot: temporaryDirectory,
      },
    )

    expect(await adapter.compose("original draft")).toBe("edited draft\n")
    expect(ordering).toEqual(["suspend", "editor", "resume"])
    expect(executor.calls[0]?.executable).toBe("mock-editor")
    expect(executor.calls[0]?.args.slice(0, -1)).toEqual([
      "--wait",
      "profile one;",
      "touch",
      "/tmp/nope",
    ])
    expect(executor.calls[0]?.options?.inheritTerminal).toBeTrue()
  })

  test("returns null and always resumes when the editor fails", async () => {
    temporaryDirectory = await mkdtemp(join(tmpdir(), "rw-platform-test-"))
    const ordering: string[] = []
    const executor = new RecordingExecutor()
    executor.handler = async () => ({ status: 9, stdout: new Uint8Array() })
    const adapter = createExternalEditorAdapter(
      {
        suspend: () => ordering.push("suspend"),
        resume: () => ordering.push("resume"),
      },
      { environment: { VISUAL: "broken-editor" }, executor, temporaryRoot: temporaryDirectory },
    )
    expect(await adapter.compose("keep me")).toBeNull()
    expect(ordering).toEqual(["suspend", "resume"])
  })

  test("uses native notification argv with escaped and bounded text", async () => {
    const mac = new RecordingExecutor()
    await createDesktopNotificationAdapter({ platform: "darwin", executor: mac }).notify({
      kind: "turn_finished",
      title: 'Rottweiler "done"',
      body: "line one\nline two",
    })
    expect(mac.calls[0]?.executable).toBe("osascript")
    expect(mac.calls[0]?.args.join(" ")).toContain('Rottweiler \\"done\\"')
    expect(mac.calls[0]?.args.join(" ")).not.toContain("\n")

    const linux = new RecordingExecutor()
    await createDesktopNotificationAdapter({ platform: "linux", executor: linux }).notify({
      kind: "approval_needed",
      title: "Approval",
      body: "bash",
    })
    expect(linux.calls[0]).toMatchObject({
      executable: "notify-send",
      args: ["--app-name=Rottweiler", "Approval", "bash"],
    })
  })

  test("opens only bounded HTTPS auth URLs through a native argv launcher", async () => {
    const executor = new RecordingExecutor()
    const adapter = createExternalUrlAdapter({ platform: "darwin", executor })
    await adapter.open("https://auth.example.test/authorize?state=one%20two")

    expect(executor.calls[0]).toMatchObject({
      executable: "open",
      args: ["-u", "https://auth.example.test/authorize?state=one%20two"],
    })
    expect(executor.calls[0]?.args).toHaveLength(2)
    await expect(adapter.open("javascript:alert(1)")).rejects.toThrow(
      "safe HTTPS",
    )
    await expect(
      adapter.open("https://user:secret@example.test/login"),
    ).rejects.toThrow("safe HTTPS")
    await expect(
      adapter.open(`https://example.test/${"x".repeat(4_096)}`),
    ).rejects.toThrow("invalid")
    expect(executor.calls).toHaveLength(1)
  })

  test("copies bounded challenge text over stdin with a Linux native fallback", async () => {
    const executor = new RecordingExecutor()
    executor.handler = async (executable) => {
      if (executable === "wl-copy") throw new Error("Wayland unavailable")
      return { status: 0, stdout: new Uint8Array() }
    }
    const adapter = createTextClipboardAdapter({ platform: "linux", executor })
    await adapter.writeText("ABCD-1234")

    expect(executor.calls.map((call) => call.executable)).toEqual([
      "wl-copy",
      "xclip",
    ])
    expect(executor.calls[1]?.args).toEqual(["-selection", "clipboard", "-in"])
    expect(executor.calls.flatMap((call) => call.args)).not.toContain(
      "ABCD-1234",
    )
    expect(new TextDecoder().decode(executor.calls[1]?.options?.stdin)).toBe(
      "ABCD-1234",
    )
    await expect(adapter.writeText(`A${"x".repeat(4_096)}`)).rejects.toThrow(
      "size limit",
    )
    await expect(adapter.writeText("code\u0000tail")).rejects.toThrow("invalid")
    expect(executor.calls).toHaveLength(2)
  })

  test("reads a bounded signature-checked Linux clipboard image with graceful fallback", async () => {
    const executor = new RecordingExecutor()
    executor.handler = async (executable) => {
      if (executable === "wl-paste") throw new Error("Wayland unavailable")
      return { status: 0, stdout: PNG }
    }
    const image = await createImagePasteAdapter({ platform: "linux", executor }).readImage()
    expect(image).toEqual({
      name: "clipboard.png",
      mediaType: "image/png",
      base64: Buffer.from(PNG).toString("base64"),
    })
    expect(executor.calls.map((call) => call.executable)).toEqual([
      "wl-paste",
      "wl-paste",
      "xclip",
    ])

    const invalid = new RecordingExecutor()
    invalid.handler = async () => ({ status: 0, stdout: Uint8Array.from([1, 2, 3]) })
    expect(
      await createImagePasteAdapter({ platform: "linux", executor: invalid }).readImage(),
    ).toBeNull()
  })

  test("reads quoted, escaped, and file URL image paths without losing spaces", async () => {
    temporaryDirectory = await mkdtemp(join(tmpdir(), "rw platform images "))
    const imagePath = join(temporaryDirectory, "screen shot.png")
    await writeFile(imagePath, PNG)
    const adapter = createImagePasteAdapter({ platform: "darwin" })

    expect((await adapter.readPath(`'${imagePath}'`))?.name).toBe("screen shot.png")
    expect((await adapter.readPath(imagePath.replaceAll(" ", "\\ ")))?.mediaType).toBe("image/png")
    expect((await adapter.readPath(new URL(`file://${imagePath}`).toString()))?.base64)
      .toBe(Buffer.from(PNG).toString("base64"))
    expect(await adapter.readPath("https://example.test/screen.png")).toBeNull()
    expect(await adapter.readPath("please review screenshot.png")).toBeNull()
    expect(await adapter.readPath("please compare src/screenshot.png")).toBeNull()
    expect(adapter.readPath("file://%ZZ/private.png"))
      .rejects.toThrow("not a valid local image path")
    expect(createImagePasteAdapter({ platform: "linux" }).readPath(
      String.raw`C:\Users\Alice\screen shot.png`,
    )).rejects.toThrow("could not be read safely")
    const linkPath = join(temporaryDirectory, "linked image.png")
    await symlink(imagePath, linkPath)
    expect(adapter.readPath(linkPath)).rejects.toThrow("could not be read safely")
    expect(adapter.readPath(join(temporaryDirectory, "missing image.png")))
      .rejects.toThrow("could not be read safely")
  })

  test("writes and validates the macOS clipboard through a private temporary PNG", async () => {
    temporaryDirectory = await mkdtemp(join(tmpdir(), "rw-platform-test-"))
    const executor = new RecordingExecutor()
    executor.handler = async (_executable, args) => {
      const script = args.join(" ")
      const match = /POSIX file \"([^\"]+)\"/.exec(script)
      if (match?.[1] === undefined)
        throw new Error("missing clipboard destination")
      await writeFile(match[1], PNG)
      return { status: 0, stdout: new Uint8Array() }
    }
    const image = await createImagePasteAdapter({
      platform: "darwin",
      executor,
      temporaryRoot: temporaryDirectory,
    }).readImage()
    expect(image?.mediaType).toBe("image/png")
    expect(image?.base64).toBe(Buffer.from(PNG).toString("base64"))
    expect(executor.calls[0]?.executable).toBe("osascript")
  })

  test("parses quoted editor argv and rejects unterminated input", () => {
    expect(parseCommandLine(`code --wait 'profile one' "two"`)).toEqual([
      "code",
      "--wait",
      "profile one",
      "two",
    ])
    expect(() => parseCommandLine("code 'unterminated")).toThrow()
  })
})
