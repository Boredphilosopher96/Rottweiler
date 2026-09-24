import { TextRenderable } from "./text"
import { BoxRenderable, type RenderContext } from "@opentui/core"

import type { RottweilerState, SubagentProjection } from "../state"
import type { RottweilerTheme } from "../theme"
import { truncateToCells } from "../render/text"
import { formatKnownCost } from "../render"
import { subagentGlyph } from "./transcript/blocks"

const MAX_STRIP_ROWS = 5
/** Admitted child waiting for a free concurrency slot. */
export const QUEUED_GLYPH = "◷"
const FALLBACK_STRIP_CONTENT_WIDTH = 96

/** What the parent keeps showing about its children while it works. */
export interface AgentsStripInput {
  /** Children the strip lists, running first; see {@link agentsStripEntries}. */
  readonly entries: readonly SubagentProjection[]
  /** Agent definition name for a child id, when the catalog knows it. */
  readonly agentName: (subagentId: string) => string | null
  /** Whether a live child is still waiting for a free child slot. */
  readonly queued?: (subagentId: string) => boolean
  /** Keycap that opens the Agents screen. */
  readonly agentsKey: string | null
  /** Keycap that detaches the child a foreground spawn or wait is blocked on. */
  readonly backgroundKey: string | null
}

/**
 * One compact line per child above the composer. Running and queued children
 * are always listed; finished ones stay (dimmed) until the caller hides them.
 */
export class AgentsStripRenderable extends BoxRenderable {
  readonly rows = new Map<string, TextRenderable>()
  readonly more: TextRenderable
  readonly footer: TextRenderable
  readonly #theme: RottweilerTheme
  readonly #onOpenSubagent: (subagentId: string) => void
  readonly #onElapsed: (() => void) | undefined
  #entries: readonly SubagentProjection[] = []
  #shown: readonly SubagentProjection[] = []
  #input: AgentsStripInput | null = null
  #rowOrder: readonly string[] = []
  #elapsedTimer: ReturnType<typeof setInterval> | null = null
  #lastRenderNowMs = Date.now()

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    onOpenSubagent: (subagentId: string) => void,
    onElapsed?: () => void,
  ) {
    super(ctx, {
      id: "agents-strip",
      width: "100%",
      height: 0,
      flexDirection: "column",
      flexShrink: 0,
      border: ["left"],
      borderStyle: "single",
      borderColor: theme.info,
      backgroundColor: theme.background,
      paddingLeft: 1,
      visible: false,
    })
    this.#theme = theme
    this.#onOpenSubagent = onOpenSubagent
    this.#onElapsed = onElapsed
    this.onSizeChange = () => this.#render(this.#lastRenderNowMs)
    this.more = new TextRenderable(ctx, {
      content: "",
      fg: theme.textMuted,
      height: 0,
      flexShrink: 0,
      visible: false,
      wrapMode: "none",
    })
    this.footer = new TextRenderable(ctx, {
      content: "",
      fg: theme.textMuted,
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
    })
    this.add(this.more)
    this.add(this.footer)
  }

  get entries(): readonly SubagentProjection[] { return this.#entries }

  update(input: AgentsStripInput, nowMs = Date.now()): void {
    this.#input = input
    this.#entries = input.entries
    this.#shown = input.entries.slice(0, MAX_STRIP_ROWS)
    const nextOrder = this.#shown.map((subagent) => subagent.projectionId)
    if (
      nextOrder.length !== this.#rowOrder.length ||
      nextOrder.some((subagentId, index) => subagentId !== this.#rowOrder[index])
    ) {
      for (const row of this.rows.values()) {
        this.remove(row)
        row.destroyRecursively()
      }
      this.rows.clear()
      this.#rowOrder = nextOrder
    }
    for (const [index, subagent] of this.#shown.entries()) {
      let row = this.rows.get(subagent.projectionId)
      if (row === undefined) {
        row = new TextRenderable(this.ctx, {
          content: "",
          height: 1,
          flexShrink: 0,
          wrapMode: "none",
        })
        this.rows.set(subagent.projectionId, row)
        this.add(row, index)
      }
      row.onMouseDown = () => this.#onOpenSubagent(subagent.subagentId)
      row.fg = input.queued?.(subagent.subagentId) === true ? this.#theme.textMuted : stripColor(this.#theme, subagent.status)
    }
    this.#render(nowMs)
    this.#syncElapsedTimer()
  }

  override destroy(): void {
    this.#entries = []
    this.#shown = []
    this.#clearElapsedTimer()
    super.destroy()
  }

  #render(nowMs: number): void {
    this.#lastRenderNowMs = nowMs
    const usableWidth = this.width <= 0
      ? FALLBACK_STRIP_CONTENT_WIDTH
      : Math.max(0, this.width - 4)
    for (const subagent of this.#shown) {
      const row = this.rows.get(subagent.projectionId)
      if (row === undefined) continue
      row.content = truncateToCells(
        agentsStripLine(subagent, this.#input?.agentName(subagent.subagentId) ?? null, nowMs,
          this.#input?.queued?.(subagent.subagentId) ?? false),
        usableWidth,
      )
    }
    const hidden = this.#entries.length - this.#shown.length
    this.more.visible = hidden > 0
    this.more.height = hidden > 0 ? 1 : 0
    this.more.content = hidden > 0 ? `… ${hidden} more` : ""
    const running = this.#entries.some((subagent) => subagent.status === "running")
    const hints = [
      ...(this.#input?.backgroundKey == null ? [] : [`${this.#input.backgroundKey} background`]),
      ...(this.#input?.agentsKey == null ? ["/agents"] : [`${this.#input.agentsKey} agents`]),
      ...(running ? [] : ["hidden after your next message"]),
    ]
    this.footer.content = truncateToCells(`╰ ${hints.join(" · ")}`, usableWidth)
    this.visible = this.#entries.length > 0
    this.height = !this.visible ? 0 : this.#shown.length + (hidden > 0 ? 1 : 0) + 1
  }

  #syncElapsedTimer(): void {
    if (!this.#shown.some((subagent) => subagent.status === "running")) {
      this.#clearElapsedTimer()
      return
    }
    if (this.#elapsedTimer !== null) return
    this.#elapsedTimer = setInterval(() => {
      this.#render(Date.now())
      this.#onElapsed?.()
    }, 1_000)
  }

  #clearElapsedTimer(): void {
    if (this.#elapsedTimer === null) return
    clearInterval(this.#elapsedTimer)
    this.#elapsedTimer = null
  }
}

/** One strip line: glyph, agent, short task, activity, elapsed, and cost. */
export function agentsStripLine(
  subagent: SubagentProjection,
  agent: string | null,
  nowMs = Date.now(),
  queued = false,
): string {
  const task = truncateToCells(subagent.task.replace(/\s+/g, " ").trim(), 48)
  const waiting = queued && subagent.status === "running"
  const status = waiting
    ? "queued"
    : subagent.status === "running"
      ? truncateToCells((subagent.activity ?? "running").replace(/\s+/g, " ").trim(), 40)
      : subagent.status.replaceAll("_", " ")
  const elapsed = subagent.status === "running" && !waiting ? formatSubagentElapsed(subagent.spawnedAtMs, nowMs) : null
  return [
    `${waiting ? QUEUED_GLYPH : subagentGlyph(subagent.status)} ${agent ?? "agent"}`,
    task,
    status,
    ...(elapsed === null ? [] : [elapsed]),
    ...optional(formatKnownCost(subagent.cost)),
  ].join(" · ")
}

function optional(value: string | null): string[] {
  return value === null ? [] : [value]
}

/**
 * Children the strip lists: every running child, then finished children the
 * user has not dismissed. `observed` holds children seen running in this
 * client, so children that finished before the session was opened stay out.
 */
export function agentsStripEntries(
  state: RottweilerState,
  hidden: ReadonlySet<string>,
  observed: ReadonlySet<string>,
): SubagentProjection[] {
  const ordered = orderedSubagents(state)
  return [
    ...ordered.filter((subagent) => subagent.status === "running"),
    ...ordered.filter((subagent) => subagent.status !== "running" &&
      observed.has(subagent.projectionId) && !hidden.has(subagent.projectionId)),
  ]
}

export function orderedSubagents(state: RottweilerState): SubagentProjection[] {
  return state.subagentOrder
    .map((subagentId) => state.subagents[subagentId])
    .filter((subagent): subagent is SubagentProjection => subagent !== undefined)
}

export function formatSubagentElapsed(spawnedAtMs: number | null, nowMs = Date.now()): string | null {
  if (spawnedAtMs === null || !Number.isFinite(spawnedAtMs)) return null
  const totalSeconds = Math.max(0, Math.floor((nowMs - spawnedAtMs) / 1_000))
  const hours = Math.floor(totalSeconds / 3_600)
  const minutes = Math.floor((totalSeconds % 3_600) / 60)
  const seconds = totalSeconds % 60
  if (hours > 0) return `${hours}h${minutes.toString().padStart(2, "0")}m${seconds.toString().padStart(2, "0")}s`
  if (minutes > 0) return `${minutes}m${seconds.toString().padStart(2, "0")}s`
  return `${seconds}s`
}

/** Bounded side-panel projection: running children first, then the newest finished ones. */
export function boundedSubagents(
  subagents: readonly SubagentProjection[],
  limit = 6,
): SubagentProjection[] {
  if (subagents.length <= limit) return [...subagents]
  const running = subagents
    .filter((subagent) => subagent.status === "running")
    .slice(0, limit)
  const remaining = limit - running.length
  if (remaining === 0) return running
  const terminal = subagents.filter((subagent) => subagent.status !== "running")
  return [...running, ...terminal.slice(-remaining)]
}

function stripColor(
  theme: RottweilerTheme,
  status: SubagentProjection["status"],
): string {
  if (status === "running") return theme.info
  if (status === "failed") return theme.error
  return theme.textMuted
}
