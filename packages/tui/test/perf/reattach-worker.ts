import type { EngineEvent } from "../../src/protocol"
import { writeFileSync } from "node:fs"

import { createEngineRuntimeFromEnvironment, type RuntimeApp } from "../../src/runtime"
import { createInitialState, engineEvent, reduceRottweilerState } from "../../src/state"


const targetSequence = process.env.ROTTWEILER_TEST_TARGET_SEQUENCE ?? "never"
const reportFile = process.env.ROTTWEILER_TEST_REPORT_FILE
let targetReached = false
let stopRuntime: (() => Promise<void>) | null = null
const receivedSequences: string[] = []

class WorkerApp implements RuntimeApp {
  state = createInitialState()

  setSessionId(_sessionId: string): void {}

  handleEvent(event: EngineEvent): void {
    this.state = reduceRottweilerState(this.state, engineEvent(event))
    if (
      "meta" in event &&
      typeof event.meta === "object" &&
      event.meta !== null &&
      "sequence_id" in event.meta &&
      typeof event.meta.sequence_id === "string"
    ) {
      receivedSequences.push(event.meta.sequence_id)
    }
    if (this.state.lastSequence === targetSequence && reportFile !== undefined) {
      targetReached = true
      writeFileSync(
        reportFile,
        `${JSON.stringify({
          lastSequence: this.state.lastSequence,
          duplicateEvents: this.state.protocol.duplicateEvents,
          invalidEvents: this.state.protocol.invalidEvents,
          gap: this.state.connection.gap,
          receivedSequences,
        })}\n`,
        { encoding: "utf8", mode: 0o600 },
      )
      queueMicrotask(() => void stopRuntime?.())
    }
  }

  setState(state: ReturnType<typeof createInitialState>): void {
    this.state = state
  }
}

const app = new WorkerApp()

const runtime = await createEngineRuntimeFromEnvironment()
if (runtime === null) {
  throw new Error("reattach worker requires an engine runtime")
}
stopRuntime = () => runtime.stop()
runtime.bind(app)
await runtime.start()

if (!targetReached) {
  throw new Error("event stream closed before the target sequence")
}
