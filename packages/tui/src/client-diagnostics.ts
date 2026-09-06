/** Fixed local measurements; no session IDs, command names or payloads enter this owner. */
export const CLIENT_STAGES = [
  "event_decode", "reply_decode", "reply_allocation", "reply_validation", "read_queue_age", "reducer", "presentation", "presentation_queue_age",
  "history_admission", "history_update", "history_layout", "history_queue_age",
  "startup_modules", "startup_renderer", "startup_first_frame", "startup_app_modules",
  "startup_configuration", "startup_parser_assets", "startup_app_mount", "startup_paint", "startup_input",
] as const
export type ClientStage = typeof CLIENT_STAGES[number]

// Milliseconds. The last bucket includes all larger durations.
const UPPER_BOUNDS_MS = [0.001, 0.004, 0.016, 0.064, 0.256, 1, 4, 16, 64, 256, 1_000, 4_000, 16_000, 60_000] as const
const MAX_COUNTER = Number.MAX_SAFE_INTEGER

interface StageCounters {
  count: number
  units: number
  totalMs: number
  maxMs: number
  readonly buckets: Float64Array
}

export interface ClientTimingSnapshot {
  readonly version: 1
  readonly bucketUpperBoundsMs: readonly number[]
  readonly stages: readonly {
    readonly stage: ClientStage
    readonly count: number
    readonly units: number
    readonly totalMs: number
    readonly maxMs: number
    readonly buckets: readonly number[]
  }[]
}

/** The composition root creates this only for an explicitly enabled diagnostic run. */
export class ClientDiagnostics {
  readonly #clock: () => number
  readonly #stages: Readonly<Record<ClientStage, StageCounters>>

  constructor(clock: () => number = performance.now.bind(performance)) {
    this.#clock = clock
    this.#stages = Object.fromEntries(CLIENT_STAGES.map(stage => [stage, {
      count: 0, units: 0, totalMs: 0, maxMs: 0, buckets: new Float64Array(UPPER_BOUNDS_MS.length + 1),
    }])) as Record<ClientStage, StageCounters>
  }

  start(): number { return this.#clock() }

  finish(stage: ClientStage, startedAt: number, units = 1): void {
    this.record(stage, this.#clock() - startedAt, units)
  }

  record(stage: ClientStage, elapsedMs: number, units = 1): void {
    if (!Number.isFinite(elapsedMs) || elapsedMs < 0 || !Number.isSafeInteger(units) || units < 0) return
    const counters = this.#stages[stage]
    counters.count = Math.min(MAX_COUNTER, counters.count + 1)
    counters.units = Math.min(MAX_COUNTER, counters.units + units)
    counters.totalMs = Math.min(MAX_COUNTER, counters.totalMs + elapsedMs)
    counters.maxMs = Math.max(counters.maxMs, elapsedMs)
    let bucket = 0
    while (bucket < UPPER_BOUNDS_MS.length && elapsedMs > UPPER_BOUNDS_MS[bucket]!) bucket += 1
    counters.buckets[bucket] = Math.min(MAX_COUNTER, counters.buckets[bucket]! + 1)
  }

  snapshot(): ClientTimingSnapshot {
    return {
      version: 1,
      bucketUpperBoundsMs: [...UPPER_BOUNDS_MS],
      stages: CLIENT_STAGES.map(stage => {
        const counters = this.#stages[stage]
        return {
          stage, count: counters.count, units: counters.units,
          totalMs: counters.totalMs, maxMs: counters.maxMs, buckets: Array.from(counters.buckets)
        }
      }),
    }
  }
}
