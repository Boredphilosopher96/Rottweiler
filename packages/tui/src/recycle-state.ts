import {
  closeSync,
  fchmodSync,
  fsyncSync,
  lstatSync,
  openSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs"
import { dirname } from "node:path"

import type { Attachment } from "./protocol"
import { MAX_ATTACHMENTS_PER_MESSAGE } from "./protocol"
import type { ComposerDraft } from "./subagent-state"
import type { HistoryViewport } from "./history/controller"
import { parseU64 } from "./transport/types"
import type { InputMode, VimFocus } from "./keybindings"

export const MAX_RECYCLE_STATE_BYTES = 8 * 1024 * 1024
export const RESTORABLE_PICKERS = [
  "palette", "keyboardHelp", "commands", "attachments", "mcp", "modes", "models",
  "providers", "permissions", "permissionMode", "trust", "queuedMessages",
  "workspaceRoots", "budgets", "sessions", "settings", "agents", "timeline", "themes",
] as const
export type RestorablePickerKind = typeof RESTORABLE_PICKERS[number]

export interface ClientComposerState extends ComposerDraft {
  readonly cursorOffset: number
  readonly selection: { readonly start: number; readonly end: number } | null
}

export interface ClientBlockState {
  readonly selectedId: string | null
  readonly expanded: readonly { readonly id: string; readonly expanded: boolean }[]
}

export interface TranscriptClientState {
  readonly blocks: ClientBlockState
  readonly tools: ClientBlockState["expanded"]
  readonly reasoning: ClientBlockState["expanded"]
}

/** Only editable client state belongs here; engine projections and credentials do not. */
export interface AppClientState {
  readonly schemaVersion: 3
  readonly sessionId: string
  readonly composer: ClientComposerState
  readonly subagentDrafts: readonly { readonly id: string; readonly draft: ComposerDraft }[]
  readonly primaryView: "conversation" | "tools"
  readonly history: HistoryViewport
  readonly toolsScrollTop: number
  readonly transcript: TranscriptClientState
  readonly tools: ClientBlockState
  readonly inputMode: InputMode
  readonly focus: Exclude<VimFocus, "picker">
  readonly theme: string
  readonly picker: {
    readonly kind: RestorablePickerKind
    readonly anchored: boolean
    readonly query: string
    readonly selectedId: string | null
    readonly scrollOffset: number
    readonly modelProviderFilter: string | null
    readonly onboarding: boolean
    readonly themeBeforePreview: string | null
  } | null
}

export function isRestorablePicker(kind: string): kind is RestorablePickerKind {
  return RESTORABLE_PICKERS.some((candidate) => candidate === kind)
}

/** Consume a private, one-shot TUI recycle handoff. Invalid files fail closed. */
export function readTuiRecycleState(path: string | undefined): AppClientState | null {
  if (path === undefined || path.length === 0) return null
  try {
    const metadata = lstatSync(path)
    if (
      metadata.isSymbolicLink() ||
      !metadata.isFile() ||
      metadata.size > MAX_RECYCLE_STATE_BYTES ||
      (metadata.mode & 0o077) !== 0 ||
      (process.getuid !== undefined && metadata.uid !== process.getuid())
    ) return null
    const parsed: unknown = JSON.parse(readFileSync(path, "utf8"))
    unlinkSync(path)
    return parseTuiRecycleState(parsed)
  } catch {
    return null
  }
}

/** Atomically persist the small TUI-only state lost during an RSS recycle. */
export function writeTuiRecycleState(path: string | undefined, state: AppClientState): boolean {
  if (path === undefined || path.length === 0) return false
  const encoded = `${JSON.stringify(state)}\n`
  if (Buffer.byteLength(encoded) > MAX_RECYCLE_STATE_BYTES) return false
  const temporary = `${path}.${process.pid}.tmp`
  let descriptor: number | null = null
  try {
    const parent = lstatSync(dirname(path))
    if (
      parent.isSymbolicLink() ||
      !parent.isDirectory() ||
      (parent.mode & 0o077) !== 0 ||
      (process.getuid !== undefined && parent.uid !== process.getuid())
    ) return false
    descriptor = openSync(temporary, "wx", 0o600)
    fchmodSync(descriptor, 0o600)
    writeFileSync(descriptor, encoded, "utf8")
    fsyncSync(descriptor)
    closeSync(descriptor)
    descriptor = null
    renameSync(temporary, path)
    return true
  } catch {
    return false
  } finally {
    if (descriptor !== null) closeSync(descriptor)
    try {
      unlinkSync(temporary)
    } catch {
      // The successful rename consumed it; a failed best-effort cleanup is inert.
    }
  }
}

export function recycleTuiIfNeeded(options: {
  readonly observedBytes: number
  readonly thresholdBytes: number
  readonly path: string | undefined
  readonly capture: () => AppClientState | null
  readonly recycle: () => void
}): boolean {
  if (options.observedBytes < options.thresholdBytes) return false
  const state = options.capture()
  if (state === null || !writeTuiRecycleState(options.path, state)) return false
  options.recycle()
  return true
}

const record = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value)
const offset = (value: unknown): value is number => Number.isSafeInteger(value) && typeof value === "number" && value >= 0
const label = (value: unknown): value is string => typeof value === "string" && value.length <= 4096
const nullableLabel = (value: unknown): value is string | null => value === null || label(value)

function parseDraft(value: unknown): ComposerDraft | null {
  if (!record(value) || typeof value.content !== "string" || !Array.isArray(value.attachments)
    || value.attachments.length > MAX_ATTACHMENTS_PER_MESSAGE) return null
  const attachments: Attachment[] = []
  for (const item of value.attachments) {
    if (!record(item) || !label(item.name) || !label(item.media_type) || !record(item.data)
      || (item.source_path !== undefined && !label(item.source_path))) return null
    const data = item.data.type === "text" && typeof item.data.content === "string"
      ? { type: "text" as const, content: item.data.content }
      : item.data.type === "inline_base64" && typeof item.data.data === "string"
        ? { type: "inline_base64" as const, data: item.data.data }
        : null
    if (data === null) return null
    attachments.push({ name: item.name, media_type: item.media_type, data,
      ...(item.source_path === undefined ? {} : { source_path: item.source_path }) })
  }
  return { content: value.content, attachments }
}

function parseExpansion(value: unknown): ClientBlockState["expanded"] | null {
  if (!Array.isArray(value) || value.length > 4096) return null
  const result: Array<{ id: string; expanded: boolean }> = []
  for (const item of value) {
    if (!record(item) || !label(item.id) || typeof item.expanded !== "boolean") return null
    result.push({ id: item.id, expanded: item.expanded })
  }
  return result
}

function parseBlocks(value: unknown): ClientBlockState | null {
  if (!record(value) || !nullableLabel(value.selectedId)) return null
  const expanded = parseExpansion(value.expanded)
  return expanded === null ? null : { selectedId: value.selectedId, expanded }
}

export function parseTuiRecycleState(value: unknown): AppClientState | null {
  if (!record(value) || value.schemaVersion !== 3 || !label(value.sessionId)
    || !record(value.composer) || !offset(value.composer.cursorOffset)
    || !offset(value.toolsScrollTop)
    || (value.primaryView !== "conversation" && value.primaryView !== "tools")
    || (value.inputMode !== "standard" && value.inputMode !== "normal" && value.inputMode !== "insert")
    || (value.focus !== "composer" && value.focus !== "transcript")
    || !label(value.theme) || !Array.isArray(value.subagentDrafts) || value.subagentDrafts.length > 256
  ) return null
  if (Buffer.byteLength(JSON.stringify(value)) > MAX_RECYCLE_STATE_BYTES) return null
  if (!record(value.history) || typeof value.history.following !== "boolean") return null
  let anchor: HistoryViewport["anchor"] = null
  if (value.history.anchor !== null) {
    const item = value.history.anchor
    if (!record(item) || typeof item.id !== "string" || parseU64(item.id) === null
      || typeof item.offset !== "number" || !Number.isSafeInteger(item.offset)) return null
    anchor = { id: item.id, offset: item.offset }
  }
  if (value.history.following && anchor !== null) return null
  const history: HistoryViewport = { following: value.history.following, anchor }
  const tools = parseBlocks(value.tools)
  if (!record(value.transcript) || tools === null) return null
  const blocks = parseBlocks(value.transcript.blocks)
  const toolExpansion = parseExpansion(value.transcript.tools)
  const reasoning = parseExpansion(value.transcript.reasoning)
  if (blocks === null || toolExpansion === null || reasoning === null) return null
  const transcript: TranscriptClientState = { blocks, tools: toolExpansion, reasoning }
  const draft = parseDraft(value.composer)
  if (draft === null) return null
  const textBytes = Buffer.byteLength(draft.content)
  if (value.composer.cursorOffset > textBytes) return null
  let selection: ClientComposerState["selection"] = null
  const rawSelection = value.composer.selection
  if (rawSelection !== null) {
    if (!record(rawSelection) || !offset(rawSelection.start) || !offset(rawSelection.end)
      || rawSelection.start > textBytes || rawSelection.end > textBytes) return null
    selection = { start: rawSelection.start, end: rawSelection.end }
  }
  const subagentDrafts: Array<{ id: string; draft: ComposerDraft }> = []
  for (const entry of value.subagentDrafts) {
    if (!record(entry) || !label(entry.id)) return null
    const childDraft = parseDraft(entry.draft)
    if (childDraft === null) return null
    subagentDrafts.push({ id: entry.id, draft: childDraft })
  }
  let picker: AppClientState["picker"] = null
  if (value.picker !== null) {
    const item = value.picker
    if (!record(item) || typeof item.kind !== "string" || !isRestorablePicker(item.kind)
      || typeof item.anchored !== "boolean" || !label(item.query) || !nullableLabel(item.selectedId)
      || !offset(item.scrollOffset) || !nullableLabel(item.modelProviderFilter)
      || typeof item.onboarding !== "boolean" || !nullableLabel(item.themeBeforePreview)) return null
    picker = { kind: item.kind, anchored: item.anchored, query: item.query, selectedId: item.selectedId,
      scrollOffset: item.scrollOffset, modelProviderFilter: item.modelProviderFilter,
      onboarding: item.onboarding, themeBeforePreview: item.themeBeforePreview }
  }
  return {
    schemaVersion: 3, sessionId: value.sessionId,
    composer: { ...draft, cursorOffset: value.composer.cursorOffset,
      selection },
    subagentDrafts, primaryView: value.primaryView === "tools" ? "tools" : "conversation",
    history, toolsScrollTop: value.toolsScrollTop, transcript, tools,
    inputMode: value.inputMode, focus: value.focus, theme: value.theme, picker,
  }
}
