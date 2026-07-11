import { spawn } from "node:child_process"
import { chmod, lstat, mkdtemp, open, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

const MAX_EDITOR_BYTES = 2 * 1024 * 1024
const MAX_CLIPBOARD_IMAGE_BYTES = 5 * 1024 * 1024
const MAX_PROCESS_DIAGNOSTIC_BYTES = 64 * 1024

export interface DesktopNotification {
  readonly title: string
  readonly body: string
  readonly kind: "turn_finished" | "approval_needed" | "question_asked" | "plugin"
}

export interface NotificationAdapter {
  notify(notification: DesktopNotification): void | Promise<void>
}

export interface EditorAdapter {
  compose(initialValue: string): Promise<string | null>
}

export interface ClipboardImage {
  readonly name: string
  readonly mediaType: string
  readonly base64: string
}

export interface ImagePasteAdapter {
  readImage(): Promise<ClipboardImage | null>
}

export interface TerminalLifecycle {
  suspend(): void
  resume(): void
}

export interface ProcessExecutionOptions {
  readonly inheritTerminal?: boolean
  readonly maximumStdoutBytes?: number
  readonly timeoutMs?: number
}

export interface ProcessExecutionResult {
  readonly status: number
  readonly stdout: Uint8Array
}

export interface ProcessExecutor {
  run(
    executable: string,
    args: readonly string[],
    options?: ProcessExecutionOptions,
  ): Promise<ProcessExecutionResult>
}

export interface PlatformAdapterOptions {
  readonly platform?: NodeJS.Platform
  readonly environment?: Readonly<Record<string, string | undefined>>
  readonly executor?: ProcessExecutor
  readonly temporaryRoot?: string
}

export const noNotifications: NotificationAdapter = {
  notify() {},
}

export const noExternalEditor: EditorAdapter = {
  async compose() {
    return null
  },
}

export const noImagePaste: ImagePasteAdapter = {
  async readImage() {
    return null
  },
}

export const systemProcessExecutor: ProcessExecutor = {
  run(executable, args, options = {}) {
    return executeProcess(executable, args, options)
  },
}

export function createExternalEditorAdapter(
  terminal: TerminalLifecycle,
  options: PlatformAdapterOptions = {},
): EditorAdapter {
  const environment = options.environment ?? process.env
  const executor = options.executor ?? systemProcessExecutor
  const temporaryRoot = options.temporaryRoot ?? tmpdir()

  return {
    async compose(initialValue) {
      let directory: string | null = null
      let suspended = false
      try {
        const editor = parseCommandLine(
          firstNonEmpty(environment.VISUAL, environment.EDITOR) ?? "vi",
        )
        if (editor.length === 0) {
          return null
        }
        directory = await mkdtemp(join(temporaryRoot, "rottweiler-editor-"))
        await chmod(directory, 0o700)
        const path = join(directory, "prompt.md")
        const handle = await open(path, "wx", 0o600)
        try {
          await handle.writeFile(initialValue, "utf8")
          await handle.sync()
        } finally {
          await handle.close()
        }

        terminal.suspend()
        suspended = true
        const executable = editor[0]
        if (executable === undefined) {
          return null
        }
        const result = await executor.run(executable, [...editor.slice(1), path], {
          inheritTerminal: true,
        })
        if (result.status !== 0) {
          return null
        }
        const metadata = await lstat(path)
        if (
          !metadata.isFile() ||
          metadata.isSymbolicLink() ||
          metadata.size > MAX_EDITOR_BYTES
        ) {
          return null
        }
        return await readFile(path, "utf8")
      } catch {
        return null
      } finally {
        if (suspended) {
          terminal.resume()
        }
        if (directory !== null) {
          await rm(directory, { recursive: true, force: true }).catch(() => {})
        }
      }
    },
  }
}

export function createDesktopNotificationAdapter(
  options: PlatformAdapterOptions = {},
): NotificationAdapter {
  const platform = options.platform ?? process.platform
  const environment = options.environment ?? process.env
  const executor = options.executor ?? systemProcessExecutor
  if (environment.ROTTWEILER_NOTIFICATIONS?.toLowerCase() === "off") {
    return noNotifications
  }

  return {
    async notify(notification) {
      const title = notificationText(notification.title, 128)
      const body = notificationText(notification.body, 512)
      try {
        if (platform === "darwin") {
          await executor.run(
            "osascript",
            [
              "-e",
              `display notification \"${escapeAppleScript(body)}\" with title \"${escapeAppleScript(title)}\"`,
            ],
            { maximumStdoutBytes: MAX_PROCESS_DIAGNOSTIC_BYTES, timeoutMs: 2_000 },
          )
        } else if (platform === "linux") {
          await executor.run("notify-send", ["--app-name=Rottweiler", title, body], {
            maximumStdoutBytes: MAX_PROCESS_DIAGNOSTIC_BYTES,
            timeoutMs: 2_000,
          })
        }
      } catch {
        // Desktop integration is intentionally best-effort. Missing binaries,
        // headless sessions, and denied notification permissions stay silent.
      }
    },
  }
}

export function createImagePasteAdapter(
  options: PlatformAdapterOptions = {},
): ImagePasteAdapter {
  const platform = options.platform ?? process.platform
  const executor = options.executor ?? systemProcessExecutor
  const temporaryRoot = options.temporaryRoot ?? tmpdir()

  return {
    async readImage() {
      try {
        if (platform === "darwin") {
          return await readMacOsClipboardImage(executor, temporaryRoot)
        }
        if (platform === "linux") {
          return await readLinuxClipboardImage(executor)
        }
      } catch {
        // Clipboard tools are optional and may be unavailable in SSH/headless
        // environments. A failed paste must leave the existing draft intact.
      }
      return null
    },
  }
}

async function readMacOsClipboardImage(
  executor: ProcessExecutor,
  temporaryRoot: string,
): Promise<ClipboardImage | null> {
  const directory = await mkdtemp(join(temporaryRoot, "rottweiler-clipboard-"))
  await chmod(directory, 0o700)
  const path = join(directory, "clipboard.png")
  try {
    const escapedPath = escapeAppleScript(path)
    const result = await executor.run(
      "osascript",
      [
        "-e",
        "set imageData to the clipboard as «class PNGf»",
        "-e",
        `set destination to open for access POSIX file \"${escapedPath}\" with write permission`,
        "-e",
        "set eof destination to 0",
        "-e",
        "write imageData to destination",
        "-e",
        "close access destination",
      ],
      { maximumStdoutBytes: MAX_PROCESS_DIAGNOSTIC_BYTES, timeoutMs: 2_000 },
    )
    if (result.status !== 0) {
      return null
    }
    const bytes = await readBoundedRegularFile(path, MAX_CLIPBOARD_IMAGE_BYTES)
    return bytes === null || !matchesImageSignature(bytes, "image/png")
      ? null
      : clipboardImage(bytes, "image/png")
  } finally {
    await rm(directory, { recursive: true, force: true }).catch(() => {})
  }
}

async function readLinuxClipboardImage(
  executor: ProcessExecutor,
): Promise<ClipboardImage | null> {
  const attempts: readonly [string, readonly string[], string][] = [
    ["wl-paste", ["--type", "image/png"], "image/png"],
    ["wl-paste", ["--type", "image/jpeg"], "image/jpeg"],
    ["xclip", ["-selection", "clipboard", "-t", "image/png", "-o"], "image/png"],
    ["xclip", ["-selection", "clipboard", "-t", "image/jpeg", "-o"], "image/jpeg"],
  ]
  for (const [executable, args, mediaType] of attempts) {
    try {
      const result = await executor.run(executable, args, {
        maximumStdoutBytes: MAX_CLIPBOARD_IMAGE_BYTES,
        timeoutMs: 2_000,
      })
      if (
        result.status === 0 &&
        result.stdout.byteLength > 0 &&
        result.stdout.byteLength <= MAX_CLIPBOARD_IMAGE_BYTES &&
        matchesImageSignature(result.stdout, mediaType)
      ) {
        return clipboardImage(result.stdout, mediaType)
      }
    } catch {
      // Try the next native clipboard implementation or media type.
    }
  }
  return null
}

async function readBoundedRegularFile(
  path: string,
  maximumBytes: number,
): Promise<Uint8Array | null> {
  const metadata = await lstat(path).catch(() => null)
  if (
    metadata === null ||
    !metadata.isFile() ||
    metadata.isSymbolicLink() ||
    metadata.size === 0 ||
    metadata.size > maximumBytes
  ) {
    return null
  }
  return await readFile(path)
}

function clipboardImage(bytes: Uint8Array, mediaType: string): ClipboardImage {
  const extension = mediaType === "image/jpeg" ? "jpg" : "png"
  return {
    name: `clipboard.${extension}`,
    mediaType,
    base64: Buffer.from(bytes).toString("base64"),
  }
}

function matchesImageSignature(bytes: Uint8Array, mediaType: string): boolean {
  if (mediaType === "image/png") {
    return (
      bytes.length >= 8 &&
      bytes[0] === 0x89 &&
      bytes[1] === 0x50 &&
      bytes[2] === 0x4e &&
      bytes[3] === 0x47 &&
      bytes[4] === 0x0d &&
      bytes[5] === 0x0a &&
      bytes[6] === 0x1a &&
      bytes[7] === 0x0a
    )
  }
  return bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff
}

function executeProcess(
  executable: string,
  args: readonly string[],
  options: ProcessExecutionOptions,
): Promise<ProcessExecutionResult> {
  return new Promise((resolve, reject) => {
    const inherit = options.inheritTerminal === true
    const maximum = options.maximumStdoutBytes ?? MAX_PROCESS_DIAGNOSTIC_BYTES
    const child = spawn(executable, [...args], {
      shell: false,
      stdio: inherit ? "inherit" : ["ignore", "pipe", "ignore"],
    })
    const chunks: Buffer[] = []
    let size = 0
    let exceeded = false
    let timedOut = false
    const timeout =
      options.timeoutMs === undefined
        ? null
        : setTimeout(() => {
            timedOut = true
            child.kill("SIGKILL")
          }, options.timeoutMs)
    child.stdout?.on("data", (chunk: Buffer | Uint8Array) => {
      size += chunk.byteLength
      if (size > maximum) {
        exceeded = true
        child.kill("SIGKILL")
        return
      }
      chunks.push(Buffer.from(chunk))
    })
    child.once("error", reject)
    child.once("close", (status) => {
      if (timeout !== null) {
        clearTimeout(timeout)
      }
      if (timedOut) {
        reject(new Error("platform command exceeded its time limit"))
        return
      }
      if (exceeded) {
        reject(new Error("platform command exceeded its bounded output limit"))
        return
      }
      resolve({ status: status ?? 1, stdout: Buffer.concat(chunks) })
    })
  })
}

export function parseCommandLine(value: string): readonly string[] {
  const args: string[] = []
  let current = ""
  let quote: "'" | '"' | null = null
  let escaping = false
  let started = false
  for (const character of value) {
    if (escaping) {
      current += character
      escaping = false
      started = true
    } else if (character === "\\" && quote !== "'") {
      escaping = true
      started = true
    } else if (quote !== null) {
      if (character === quote) {
        quote = null
      } else {
        current += character
      }
      started = true
    } else if (character === "'" || character === '"') {
      quote = character
      started = true
    } else if (/\s/.test(character)) {
      if (started) {
        args.push(current)
        current = ""
        started = false
      }
    } else {
      current += character
      started = true
    }
  }
  if (escaping || quote !== null) {
    throw new Error("editor command contains an unterminated escape or quote")
  }
  if (started) {
    args.push(current)
  }
  return args
}

function firstNonEmpty(...values: readonly (string | undefined)[]): string | null {
  for (const value of values) {
    if (value !== undefined && value.trim().length > 0) {
      return value
    }
  }
  return null
}

function notificationText(value: string, maximumCharacters: number): string {
  return value.replace(/[\r\n\0]+/g, " ").slice(0, maximumCharacters)
}

function escapeAppleScript(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/[\r\n\0]+/g, " ")
}
