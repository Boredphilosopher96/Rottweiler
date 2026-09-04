import { createInitialState } from "../../../packages/tui/src/state/model"
import { reduceWireEvent } from "../../../packages/tui/src/state/reducer"
import { normalizeWireEngineEvent } from "../../../packages/tui/src/transport/types"

const event = normalizeWireEngineEvent({ type: "command_acknowledged" })
if (event === null) throw new Error("Behavior changed: malformed event was rejected")

let failure: unknown
try {
  reduceWireEvent(createInitialState(), event)
} catch (error) {
  failure = error
}

if (!(failure instanceof TypeError)) {
  throw new Error("Behavior changed: expected the known-event reducer to throw TypeError")
}
console.log(`REPRODUCED: normalization accepted a malformed known event; reducer threw ${failure.message}`)
