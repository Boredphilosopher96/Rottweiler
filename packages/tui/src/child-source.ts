import type { EngineEvent } from "./protocol"
import { EngineProtocolError } from "./transport/errors"
import { parseU64 } from "./transport/types"

/** A body-free progress notification names the canonical child revision it invalidates. */
export function childProgressSource(event: Extract<EngineEvent, { type: "subagent_progress" }>): string | null {
  const sequence = event.child_sequence ?? null
  if ((sequence !== null && parseU64(sequence) === null) || (event.event === null && sequence === null)) {
    throw new EngineProtocolError("child source invalidation requires a canonical sequence")
  }
  return sequence
}
