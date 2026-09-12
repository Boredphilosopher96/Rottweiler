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
