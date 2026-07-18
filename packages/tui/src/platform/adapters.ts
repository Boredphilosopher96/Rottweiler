import { spawn } from "node:child_process"
import { constants as fsConstants } from "node:fs"
import { chmod, lstat, mkdtemp, open, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { basename, extname, join } from "node:path"
import { fileURLToPath } from "node:url"

const MAX_EDITOR_BYTES = 2 * 1024 * 1024
const MAX_CLIPBOARD_IMAGE_BYTES = 5 * 1024 * 1024
const MAX_PROCESS_DIAGNOSTIC_BYTES = 64 * 1024
const MAX_PROVIDER_AUTH_URL_BYTES = 4 * 1024
// Transcript selections routinely span multiple code blocks. Keep copying
// bounded without imposing the tiny challenge-code limit used by the original
// adapter.
const MAX_CLIPBOARD_TEXT_BYTES = 1024 * 1024

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
  readPath(value: string): Promise<ClipboardImage | null>
}

export interface ExternalUrlAdapter {
  open(url: string): Promise<void>
}

export interface TextClipboardAdapter {
  writeText(value: string): Promise<void>
}

export interface TerminalLifecycle {
  suspend(): void
  resume(): void
}

export interface KittyKeyboardRendererLifecycle extends TerminalLifecycle {
  disableKittyKeyboard(): void
}

export interface ProcessExecutionOptions {
  readonly inheritTerminal?: boolean
  readonly maximumStdoutBytes?: number
  readonly stdin?: Uint8Array
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
  async readPath() {
    return null
  },
}

export const noExternalUrl: ExternalUrlAdapter = {
  async open() {
    throw new Error("opening a browser is unavailable")
  },
}

export const noTextClipboard: TextClipboardAdapter = {
  async writeText() {
    throw new Error("text clipboard access is unavailable")
  },
}

export const systemProcessExecutor: ProcessExecutor = {
  run(executable, args, options = {}) {
    return executeProcess(executable, args, options)
  },
}

/** Releases terminal keyboard ownership before an inherited foreground process. */
export function createTerminalHandover(
  renderer: KittyKeyboardRendererLifecycle,
): TerminalLifecycle {
  return {
    suspend() {
      // OpenTUI's native resume restores its configured kitty flags. Pop the
      // protocol while the native output channel is still active, before the
      // renderer suspends that channel and gives stdin to the child.
      renderer.disableKittyKeyboard()
      renderer.suspend()
    },
    resume() {
      renderer.resume()
    },
  }
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

/** Opens a validated provider-auth URL through an argv-only native launcher. */
export function createExternalUrlAdapter(
  options: PlatformAdapterOptions = {},
): ExternalUrlAdapter {
  const platform = options.platform ?? process.platform
  const executor = options.executor ?? systemProcessExecutor
  return {
    async open(source) {
      const url = providerAuthUrl(source)
      const invocation =
        platform === "darwin"
          ? (["open", ["-u", url]] as const)
          : platform === "linux"
            ? (["xdg-open", [url]] as const)
            : platform === "win32"
              ? ([
                  "rundll32.exe",
                  ["url.dll,FileProtocolHandler", url],
                ] as const)
              : null
      if (invocation === null) {
        throw new Error("opening a browser is unsupported on this platform")
      }
      const result = await executor.run(invocation[0], invocation[1], {
        maximumStdoutBytes: MAX_PROCESS_DIAGNOSTIC_BYTES,
        timeoutMs: 5_000,
      })
      if (result.status !== 0) throw new Error("the browser launcher failed")
    },
  }
}

/** Writes bounded challenge text through stdin, never through a shell or argv. */
export function createTextClipboardAdapter(
  options: PlatformAdapterOptions = {},
): TextClipboardAdapter {
  const platform = options.platform ?? process.platform
  const executor = options.executor ?? systemProcessExecutor
  return {
    async writeText(value) {
      const bytes = clipboardText(value)
      const executionOptions = {
        maximumStdoutBytes: MAX_PROCESS_DIAGNOSTIC_BYTES,
        stdin: bytes,
        timeoutMs: 2_000,
      } as const
      if (platform === "darwin") {
        const result = await executor.run("pbcopy", [], executionOptions)
        if (result.status !== 0) throw new Error("the clipboard writer failed")
        return
      }
      if (platform === "linux") {
        for (const [executable, args] of [
          ["wl-copy", ["--type", "text/plain;charset=utf-8"]],
          ["xclip", ["-selection", "clipboard", "-in"]],
        ] as const) {
          try {
            const result = await executor.run(
              executable,
              args,
              executionOptions,
            )
            if (result.status === 0) return
          } catch {
            // Try the next native clipboard implementation.
          }
        }
        throw new Error("no supported clipboard writer is available")
      }
      if (platform === "win32") {
        const result = await executor.run("clip.exe", [], executionOptions)
        if (result.status !== 0) throw new Error("the clipboard writer failed")
        return
      }
      throw new Error("text clipboard access is unsupported on this platform")
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
    async readPath(value) {
      const candidate = pastedFilePath(value, platform)
      if (candidate === null) return null
      const { path, explicit } = candidate
      const mediaType = imageMediaTypeForPath(path)
      if (mediaType === null) return null
      const bytes = await readBoundedRegularFile(path, MAX_CLIPBOARD_IMAGE_BYTES)
      if (bytes === null || !matchesImageSignature(bytes, mediaType)) {
        if (!explicit) return null
        throw new Error(
          "That image path could not be read safely. Use a regular PNG, JPEG, GIF, or WebP under 5 MiB.",
        )
      }
      return clipboardImage(bytes, mediaType, basename(path))
    },
  }
}

function pastedFilePath(
  value: string,
  platform: NodeJS.Platform,
): { readonly path: string; readonly explicit: boolean } | null {
  const trimmed = value.trim()
  if (trimmed.length === 0 || trimmed.includes("\n") || /^https?:\/\//i.test(trimmed)) return null
  const quoted = (trimmed.startsWith('"') && trimmed.endsWith('"')) ||
    (trimmed.startsWith("'") && trimmed.endsWith("'"))
  const raw = quoted ? trimmed.slice(1, -1) : trimmed
  if (raw.startsWith("file://")) {
    try {
      return { path: fileURLToPath(raw), explicit: true }
    } catch {
      throw new Error("That file URL is not a valid local image path.")
    }
  }
  const windowsAbsolute = /^[A-Za-z]:[\\/]/.test(raw) || raw.startsWith("\\\\")
  const escapedWhitespace = /\\\s/.test(raw)
  const path = platform === "win32" || windowsAbsolute ? raw : raw.replace(/\\(.)/g, "$1")
  const hasWhitespace = /\s/.test(path)
  const explicit = quoted ||
    windowsAbsolute ||
    path.startsWith("/") ||
    path.startsWith("./") ||
    path.startsWith("../") ||
    path.startsWith("~/") ||
    escapedWhitespace ||
    (!hasWhitespace && (path.includes("/") || (platform === "win32" && path.includes("\\"))))
  return { path, explicit }
}

function imageMediaTypeForPath(path: string): string | null {
  switch (extname(path).toLowerCase()) {
    case ".png": return "image/png"
    case ".jpg":
    case ".jpeg": return "image/jpeg"
    case ".gif": return "image/gif"
    case ".webp": return "image/webp"
    default: return null
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
  const before = await lstat(path).catch(() => null)
  if (before === null || !before.isFile() || before.isSymbolicLink()) return null
  const noFollow = typeof fsConstants.O_NOFOLLOW === "number" ? fsConstants.O_NOFOLLOW : 0
  const file = await open(path, fsConstants.O_RDONLY | noFollow).catch(() => null)
  if (file === null) return null
  try {
    const metadata = await file.stat()
    if (
      !metadata.isFile() ||
      metadata.size === 0 ||
      metadata.size > maximumBytes ||
      (before.ino !== 0 && metadata.ino !== before.ino) ||
      (before.dev !== 0 && metadata.dev !== before.dev)
    ) return null
    const expected = Number(metadata.size)
    const target = Buffer.allocUnsafe(expected)
    let offset = 0
    while (offset < expected) {
      const { bytesRead } = await file.read(target, offset, expected - offset, offset)
      if (bytesRead === 0) return null
      offset += bytesRead
    }
    return target
  } finally {
    await file.close().catch(() => {})
  }
}

function clipboardImage(
  bytes: Uint8Array,
  mediaType: string,
  name?: string,
): ClipboardImage {
  const extension = mediaType === "image/jpeg" ? "jpg" : mediaType.slice("image/".length)
  return {
    name: name ?? `clipboard.${extension}`,
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
  if (mediaType === "image/jpeg") {
    return bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff
  }
  if (mediaType === "image/gif") {
    const header = Buffer.from(bytes.subarray(0, 6)).toString("ascii")
    return header === "GIF87a" || header === "GIF89a"
  }
  return mediaType === "image/webp" && bytes.length >= 12 &&
    Buffer.from(bytes.subarray(0, 4)).toString("ascii") === "RIFF" &&
    Buffer.from(bytes.subarray(8, 12)).toString("ascii") === "WEBP"
}

function providerAuthUrl(source: string): string {
  const bytes = new TextEncoder().encode(source)
  if (
    bytes.length === 0 ||
    bytes.length > MAX_PROVIDER_AUTH_URL_BYTES ||
    source.trim() !== source ||
    /[\u0000-\u001f\u007f]/.test(source)
  ) {
    throw new Error("provider authentication URL is invalid")
  }
  let parsed: URL
  try {
    parsed = new URL(source)
  } catch {
    throw new Error("provider authentication URL is invalid")
  }
  const normalized = parsed.toString()
  if (
    parsed.protocol !== "https:" ||
    parsed.hostname.length === 0 ||
    parsed.username.length > 0 ||
    parsed.password.length > 0 ||
    new TextEncoder().encode(normalized).length > MAX_PROVIDER_AUTH_URL_BYTES
  ) {
    throw new Error("provider authentication URL is not a safe HTTPS URL")
  }
  return normalized
}

function clipboardText(value: string): Uint8Array {
  const bytes = new TextEncoder().encode(value)
  if (
    bytes.length === 0 ||
    bytes.length > MAX_CLIPBOARD_TEXT_BYTES ||
    /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(value)
  ) {
    throw new Error("clipboard text is invalid or exceeds its size limit")
  }
  return bytes
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
      stdio: inherit
        ? "inherit"
        : [options.stdin === undefined ? "ignore" : "pipe", "pipe", "ignore"],
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
    if (!inherit && options.stdin !== undefined) {
      child.stdin?.on("error", () => {
        // A native clipboard process may exit before consuming stdin. Its
        // process status remains the bounded, user-facing failure signal.
      })
      child.stdin?.end(options.stdin)
    }
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
