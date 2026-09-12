/** Finite raw samples: renderer compute, scheduled cadence and acknowledged input stay distinct. */
export class FrameSamples {
  readonly frames: { ordinal: number; phase: string; scheduledMs: number; startedMs: number; computeMs: number; lateMs: number; missedSlots: number; inputMs: number | null; retainedBytes: number }[] = []
  readonly intervalMs = 1000 / 60
  constructor(readonly maximum = 1200) {
    if (!Number.isSafeInteger(maximum) || maximum < 1 || maximum > 1200) throw new Error("invalid joined frame capacity")
  }
  record(value: Omit<FrameSamples["frames"][number], "ordinal" | "lateMs" | "missedSlots">): void {
    if (this.frames.length >= this.maximum) throw new Error("joined frame sample capacity exceeded")
    if (![value.scheduledMs, value.startedMs, value.computeMs, value.retainedBytes].every(number => Number.isFinite(number) && number >= 0)
      || (value.inputMs !== null && (!Number.isFinite(value.inputMs) || value.inputMs < 0))
      || !Number.isSafeInteger(value.retainedBytes) || !value.phase || value.phase.length > 64) throw new Error("invalid joined frame observation")
    const lateMs = Math.max(0, value.startedMs - value.scheduledMs)
    this.frames.push({ ...value, ordinal: this.frames.length, lateMs, missedSlots: Math.floor(lateMs / this.intervalMs) })
  }
  summary() {
    const percentile = (values: number[], p: number) => {
      values.sort((a, b) => a - b)
      return values[Math.ceil(values.length * p) - 1] ?? null
    }
    const input = this.frames.flatMap(frame => frame.inputMs === null ? [] : [frame.inputMs])
    return { frames: this.frames.length, cadenceMs: this.intervalMs,
      computeP95Ms: percentile(this.frames.map(frame => frame.computeMs), .95),
      computeP999Ms: percentile(this.frames.map(frame => frame.computeMs), .999),
      inputP99Ms: percentile(input, .99), inputSamples: input.length,
      missedCadenceSlots: this.frames.reduce((sum, frame) => sum + frame.missedSlots, 0),
      maximumRetainedBytes: Math.max(0, ...this.frames.map(frame => frame.retainedBytes)),
      warmupExcluded: 0, inputMeaning: "native key dispatch through captured native frame", cadenceMeaning: "requested 60Hz native sink; not a physical monitor acknowledgement" }
  }
}
