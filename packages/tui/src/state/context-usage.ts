import type { ContextSnapshot } from "../protocol"
import type { ContextUsageProjection, RottweilerState } from "./model"

/** Live capacity is independent of the captured, complete inspector snapshot. */
export function contextUsage(snapshot: ContextSnapshot): ContextUsageProjection {
  const { through, turn_id, stable_prefix_hash, used_tokens, usable_tokens, reserved_tokens,
    context_window_known, context_window_reason } = snapshot
  return { through, turn_id, stable_prefix_hash, used_tokens, usable_tokens, reserved_tokens,
    context_window_known, ...(context_window_reason === undefined ? {} : { context_window_reason }) }
}

export function statusContext(state: RottweilerState): ContextUsageProjection | null {
  return state.contextUsage ?? state.context
}

/** Thresholds use the engine's reserved-output-adjusted capacity, never a guessed limit. */
export function contextWarning(usage: ContextUsageProjection | null): string | null {
  if (!usage?.context_window_known || Number(usage.usable_tokens) <= 0) return null
  const percent = Number(usage.used_tokens) / Number(usage.usable_tokens) * 100
  return percent >= 85 ? "Context near limit · inspect /context or compact with /compact"
    : percent >= 70 ? "Context filling · inspect /context; compaction makes room"
    : null
}
