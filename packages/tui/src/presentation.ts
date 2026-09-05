import type { EngineEvent } from "./protocol"
import type { ClientDiagnostics } from "./client-diagnostics"
export interface PresentationFrameScheduler {
  schedule(callback: () => void, delayMs: number): unknown
  cancel(handle: unknown): void
}

interface PresentationControllerOptions<T> {
  readonly diagnostics?: ClientDiagnostics | undefined
  readonly scheduler: PresentationFrameScheduler | undefined
  readonly destroyed: () => boolean
  readonly present: (pending: readonly T[], dirty: boolean) => void
  readonly afterPresent: (item: T) => void
}

export class PresentationController<T> {
  readonly #options: PresentationControllerOptions<T>
  #queue: T[] = []
  #queuedAt: number | undefined
  #dirty = false
  #frameHandle: unknown | null = null
  #presenting = false
  #suspended = false
  #coalesceWhileSuspended = false
  #lastFlushAt = performance.now() - 16

  constructor(options: PresentationControllerOptions<T>) {
    this.#options = options
  }

  enqueue(item: T, deferToFrame: boolean): void {
    this.#queuedAt ??= this.#options.diagnostics?.start()
    if (this.#suspended && this.#coalesceWhileSuspended) {
      this.#queue = [item]
      return
    }
    this.#queue.push(item)
    if (this.#suspended) return
    if (deferToFrame) this.#scheduleFrame()
    else this.flush()
  }

  markDirty(deferToFrame: boolean): void {
    this.#queuedAt ??= this.#options.diagnostics?.start()
    this.#dirty = true
    if (this.#suspended) return
    if (deferToFrame) this.#scheduleFrame()
    else this.flush()
  }

  flushBeforeStateChange(): void {
    if (!this.#suspended && !this.#presenting && this.#queue.length > 0) this.flush()
  }

  flush(): void {
    if (this.#suspended) return
    this.#cancelFrame()
    if (this.#options.destroyed() || (this.#queue.length === 0 && !this.#dirty)) return
    const startedAt = this.#options.diagnostics?.start()
    if (startedAt !== undefined && this.#queuedAt !== undefined) {
      this.#options.diagnostics?.record("presentation_queue_age", startedAt - this.#queuedAt)
    }
    this.#queuedAt = undefined
    const pending = this.#queue
    this.#queue = []
    const dirty = this.#dirty
    this.#dirty = false
    this.#presenting = true
    try {
      this.#options.present(pending, dirty)
    } finally {
      this.#presenting = false
      if (startedAt !== undefined) this.#options.diagnostics?.finish("presentation", startedAt, pending.length)
    }
    this.#lastFlushAt = performance.now()
    for (const item of pending) this.#options.afterPresent(item)
  }

  destroy(): void {
    this.#queuedAt = undefined
    this.#cancelFrame()
    this.#queue = []
    this.#dirty = false
  }

  suspend(coalesce = false): void {
    this.#suspended = true
    this.#coalesceWhileSuspended = coalesce
    if (coalesce && this.#queue.length > 1) this.#queue = [this.#queue.at(-1)!]
    this.#cancelFrame()
  }

  resume(): void {
    if (!this.#suspended) return
    this.#suspended = false
    this.#coalesceWhileSuspended = false
    this.flush()
  }

  #scheduleFrame(): void {
    if (this.#frameHandle !== null) return
    const scheduler = this.#options.scheduler
    if (scheduler === undefined) {
      const elapsed = performance.now() - this.#lastFlushAt
      // Show the first token after an idle frame immediately, then coalesce
      // deltas that arrive inside the active 16 ms presentation window.
      if (elapsed >= 16) {
        this.flush()
        return
      }
      this.#frameHandle = setTimeout(() => this.flush(), Math.max(0, 16 - elapsed))
      return
    }
    this.#frameHandle = scheduler.schedule(() => this.flush(), 16)
  }

  #cancelFrame(): void {
    const handle = this.#frameHandle
    if (handle === null) return
    this.#frameHandle = null
    const scheduler = this.#options.scheduler
    if (scheduler === undefined) clearTimeout(handle as ReturnType<typeof setTimeout>)
    else scheduler.cancel(handle)
  }
}

const IMMEDIATE_PRESENTATION_EVENTS = new Set<EngineEvent["type"]>([
  "command_acknowledged",
  "context_snapshot_ready",
  "cost_snapshot_ready",
  "session_review_ready",
  "session_review_updated",
  "prompt_dump_ready",
  "session_replay_completed",
  "session_history_ready",
  "session_forked",
  "session_exported",
  "sessions_listed",
  "subagents_listed",
  "command_descriptors_listed",
  "models_listed",
  "modes_listed",
  "settings_listed",
  "permissions_listed",
  "mcp_servers_listed",
  "runtime_services_listed",
  "workspace_files_found",
  "workspace_roots_changed",
  "workspace_status_ready",
  "sessions_search_ready",
  "workspace_file_preview_ready",
  "workspace_diff_ready",
  "host_shutdown",
  "ui_notification",
  "conversation_rewound",
  "conversation_turn_committed",
  "tool_approval_needed",
  "question_asked",
  "question_answered",
  "tool_call_started",
  "tool_call_finished",
  "tool_diff_ready",
  "tool_output_pruned",
  "turn_started",
  "turn_finished",
  "user_message_accepted",
  "message_queued",
  "queued_message_removed",
  "queued_messages_cleared",
  "user_shell_state_changed",
  "command_finished",
  "mode_changed",
  "model_changed",
  "model_context_cleared",
  "driver_changed",
  "permission_mode_changed",
  "budget_status_changed",
  "context_item_pinned",
  "context_item_evicted",
  "compaction_started",
  "compaction_finished",
  "compaction_failed",
  "compaction_attempt_started",
  "compaction_attempt_finished",
  "plan_submitted",
  "plan_reviewed",
  "subagent_spawned",
  "subagent_finished",
  "provider_configured",
  "provider_activation_finished",
  "provider_auth_started",
  "provider_auth_finished",
  "mcp_server_approval_reviewed",
  "plugin_message_injected",
  "plugin_status_changed",
  "session_created",
  "session_title_updated",
  "guard_triggered",
  "hook_failed",
  "error",
])

export function deferPresentationForEvent(
  event: { readonly type: EngineEvent["type"] },
): boolean {
  return !IMMEDIATE_PRESENTATION_EVENTS.has(event.type)
}
