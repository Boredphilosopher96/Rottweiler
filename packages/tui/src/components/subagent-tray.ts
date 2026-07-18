import { BoxRenderable, TextRenderable, type RenderContext } from "@opentui/core"

import type { RottweilerState, SubagentProjection } from "../state"
import type { RottweilerTheme } from "../theme"
import { subagentGlyph } from "./transcript"

const MAX_TRAY_SUBAGENTS = 6

export class SubagentTrayRenderable extends BoxRenderable {
  readonly rows = new Map<string, TextRenderable>()
  readonly more: TextRenderable
  readonly footer: TextRenderable
  readonly #theme: RottweilerTheme
  readonly #onOpenSubagent: (subagentId: string) => void
  readonly #onElapsed: (() => void) | undefined
  #subagents: readonly SubagentProjection[] = []
  #total = 0
  #rowOrder: readonly string[] = []
  #elapsedTimer: ReturnType<typeof setInterval> | null = null

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    onOpenSubagent: (subagentId: string) => void,
    onElapsed?: () => void,
  ) {
    super(ctx, {
      id: "subagent-tray",
      width: "100%",
      height: 0,
      flexDirection: "column",
      flexShrink: 0,
      border: true,
      borderStyle: "single",
      borderColor: theme.border,
      backgroundColor: theme.panel,
      paddingX: 1,
      visible: false,
    })
    this.#theme = theme
    this.#onOpenSubagent = onOpenSubagent
    this.#onElapsed = onElapsed
    this.more = new TextRenderable(ctx, {
      content: "",
      fg: theme.muted,
      height: 0,
      flexShrink: 0,
      visible: false,
      wrapMode: "none",
    })
    this.footer = new TextRenderable(ctx, {
      content: "ctrl+g inspect · click a row to open",
      fg: theme.muted,
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
    })
    this.add(this.more)
    this.add(this.footer)
  }

  update(state: RottweilerState, nowMs = Date.now()): void {
    const subagents = subagentsForTray(state)
    this.#total = subagents.length
    this.#subagents = boundedTraySubagents(subagents)
    const nextOrder = this.#subagents.map((subagent) => subagent.projectionId)
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
    for (const [index, subagent] of this.#subagents.entries()) {
      let row = this.rows.get(subagent.projectionId)
      if (row === undefined) {
        row = new TextRenderable(this.ctx, {
          content: "",
          fg: this.#theme.info,
          height: 1,
          flexShrink: 0,
          wrapMode: "none",
        })
        this.rows.set(subagent.projectionId, row)
        this.add(row, index)
      }
      row.onMouseDown = () => this.#onOpenSubagent(subagent.subagentId)
      row.fg = subagentColor(this.#theme, subagent.status)
    }
    this.#render(nowMs)
    this.#syncElapsedTimer()
  }

  override destroy(): void {
    this.#clearElapsedTimer()
    super.destroy()
  }

  #render(nowMs: number): void {
    for (const subagent of this.#subagents) {
      const row = this.rows.get(subagent.projectionId)
      if (row === undefined) continue
      const activity = singleLine(subagent.activity ?? subagent.status.replaceAll("_", " "), 72)
      const elapsed = subagent.status === "running"
        ? formatSubagentElapsed(subagent.spawnedAtMs, nowMs)
        : null
      row.content = `${subagentGlyph(subagent.status)} ${singleLine(subagent.task, 48)} · ${activity}${elapsed === null ? "" : ` · ${elapsed}`}`
    }
    const hidden = this.#total - this.#subagents.length
    this.more.visible = hidden > 0
    this.more.height = hidden > 0 ? 1 : 0
    this.more.content = hidden > 0 ? `… ${hidden} more · ctrl+g` : ""
    this.visible = this.#total > 0
    this.height = this.#total === 0 ? 0 : this.#subagents.length + (hidden > 0 ? 1 : 0) + 3
  }

  #syncElapsedTimer(): void {
    const running = this.#subagents.some((subagent) => subagent.status === "running")
    if (!running) {
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

export function subagentsForTray(state: RottweilerState): SubagentProjection[] {
  const currentTurnId = state.streamingTail?.turnId ?? Object.values(state.turns)
    .filter((turn) => turn.status === "running")
    .at(-1)?.turnId ?? null
  return state.subagentOrder
    .map((subagentId) => state.subagents[subagentId])
    .filter(
      (subagent): subagent is SubagentProjection =>
        subagent !== undefined &&
        (subagent.status === "running" || subagent.parentTurnId === currentTurnId),
    )
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

function boundedTraySubagents(
  subagents: readonly SubagentProjection[],
): SubagentProjection[] {
  if (subagents.length <= MAX_TRAY_SUBAGENTS) return [...subagents]
  const running = subagents
    .filter((subagent) => subagent.status === "running")
    .slice(0, MAX_TRAY_SUBAGENTS)
  const remaining = MAX_TRAY_SUBAGENTS - running.length
  if (remaining === 0) return running
  const terminal = subagents.filter((subagent) => subagent.status !== "running")
  return [...running, ...terminal.slice(-remaining)]
}

function subagentColor(
  theme: RottweilerTheme,
  status: SubagentProjection["status"],
): string {
  if (status === "failed") return theme.danger
  if (status === "completed") return theme.success
  if (status === "running") return theme.info
  return theme.warning
}

function singleLine(value: string, limit: number): string {
  const compact = value.replace(/\s+/g, " ").trim()
  return compact.length <= limit ? compact : `${compact.slice(0, Math.max(1, limit - 1))}…`
}
