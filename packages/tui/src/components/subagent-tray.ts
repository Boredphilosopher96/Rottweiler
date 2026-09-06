import { BoxRenderable, TextRenderable, type RenderContext } from "@opentui/core"

import type { RottweilerState, SubagentProjection } from "../state"
import type { RottweilerTheme } from "../theme"
import { truncateToCells } from "../render/text"
import { subagentGlyph } from "./transcript/blocks"

const MAX_TRAY_SUBAGENTS = 6
const FALLBACK_TRAY_CONTENT_WIDTH = 96

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
  #lastRenderNowMs = Date.now()
  #presentationEnabled = true

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
      content: "╰ Ctrl+G inspect · click a row to open",
      fg: theme.textMuted,
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

  setPresentationEnabled(enabled: boolean): void {
    if (this.#presentationEnabled === enabled) return
    this.#presentationEnabled = enabled
    this.#render(this.#lastRenderNowMs)
    this.#syncElapsedTimer()
  }

  override destroy(): void {
    this.#subagents = []
    this.#clearElapsedTimer()
    super.destroy()
  }

  #render(nowMs: number): void {
    this.#lastRenderNowMs = nowMs
    const usableWidth = this.width <= 0
      ? FALLBACK_TRAY_CONTENT_WIDTH
      : Math.max(0, this.width - 4)
    for (const subagent of this.#subagents) {
      const row = this.rows.get(subagent.projectionId)
      if (row === undefined) continue
      const activity = truncateToCells(
        (subagent.activity ?? subagent.status.replaceAll("_", " ")).replace(/\s+/g, " ").trim(),
        72,
      )
      const task = truncateToCells(subagent.task.replace(/\s+/g, " ").trim(), 48)
      const elapsed = subagent.status === "running"
        ? formatSubagentElapsed(subagent.spawnedAtMs, nowMs)
        : null
      const content = `${subagentGlyph(subagent.status)} ${task} · ${activity}${elapsed === null ? "" : ` · ${elapsed}`}`
      row.content = truncateToCells(content, usableWidth)
    }
    const hidden = this.#total - this.#subagents.length
    this.more.visible = hidden > 0
    this.more.height = hidden > 0 ? 1 : 0
    this.more.content = hidden > 0 ? `… ${hidden} more · Ctrl+G` : ""
    this.visible = this.#presentationEnabled && this.#total > 0
    this.height = !this.visible ? 0 : this.#subagents.length + (hidden > 0 ? 1 : 0) + 1
  }

  #syncElapsedTimer(): void {
    const running = this.#presentationEnabled &&
      this.#subagents.some((subagent) => subagent.status === "running")
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
  if (status === "failed") return theme.error
  if (status === "completed") return theme.success
  if (status === "running") return theme.info
  return theme.warning
}
