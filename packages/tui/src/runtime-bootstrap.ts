import { retainedJsonBytes } from "./retained-json"
import { MAX_SESSION_CONTROLS_PREPARED_BYTES, MAX_SESSION_STATE_PREPARED_BYTES, MAX_SESSION_CHILDREN_PREPARED_BYTES } from "../../../protocol/types"
import { CLIENT_TASK_REPLY_BYTES, type ClientAllocationLease, type ClientAllocationDomain } from "./client-allocation"
import type { ClientCommand, CommandReply, EngineEvent } from "./protocol"
import type { ReplyAllocation } from "./transport/reply-allocation"
import type { ClientCache } from "./history/cache"
import type { HistoryCacheValue } from "./history/controller"
import { collectLiveTail, LiveTailSnapshot, TailChanged } from "./history/live-tail"
import { directSessionRead } from "./session-reader"
import { createInitialState, type RottweilerState } from "./state"
import { readSessionState } from "./state/recovery"
import { readControls } from "./state/controls"
import { restoreChildren } from "./state/children-recovery"
import { readTodos } from "./state/todos"
import { installLiveTail, minimumSequence } from "./state/tail-recovery"
import { EngineProtocolError } from "./transport/errors"

type ReadCommand = Extract<ClientCommand, { type: "get_session_state" | "get_session_controls" | "read_session_children" | "get_todos" | "read_transcript_tail" }>
export type BootstrapPost = (command: ReadCommand, signal: AbortSignal, allocation: ReplyAllocation) => Promise<CommandReply>

/** Credit follows the installed source projection through its last renderer reference. */
export class SessionBootstrap {
  #state: RottweilerState | null
  #tail: LiveTailSnapshot | null
  #allocations: ClientAllocationLease[] | null
  constructor(state: RottweilerState, tail: LiveTailSnapshot, allocations: ClientAllocationLease[]) {
    this.#state = state; this.#tail = tail; this.#allocations = allocations
  }
  takeState(): RottweilerState {
    if (this.#state === null) throw new Error("session bootstrap is released")
    const state = this.#state
    this.#state = null
    return state
  }
  release(): void {
    this.#state = null
    this.#tail?.release(); this.#tail = null
    for (const allocation of this.#allocations ?? []) allocation.release()
    this.#allocations = null
  }
}

/** Independent source cuts converge by replaying from their minimum, never by relabelling a snapshot. */
export async function collectSessionBootstrap(
  post: BootstrapPost, meta: () => ClientCommand["meta"], cache: ClientCache<HistoryCacheValue>, sessionId: string, signal: AbortSignal,
): Promise<SessionBootstrap> {
  const allocations: ClientAllocationLease[] = []
  let tail: LiveTailSnapshot | null = null
  const read = async <Kind extends EngineEvent["type"]>(command: ReadCommand, kind: Kind, domain: ClientAllocationDomain, maximum: number): Promise<Extract<EngineEvent, { type: Kind }>> => {
    const allocation = cache.allocations.reserve(domain, 0)
    allocations.push(allocation)
    const reply = await post(command, signal, { admit(bytes) {
      if (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > maximum) throw new EngineProtocolError("bootstrap reply exceeds its source-owned allocation limit")
      allocation.resize(Math.max(allocation.bytes, bytes))
    } })
    signal.throwIfAborted()
    const event = reply.type === "read" ? reply.events[0] : undefined
    if (reply.outcome.type !== "accepted" || reply.type !== "read" || reply.events.length !== 1
      || event?.type !== kind || !("session_id" in event) || event.session_id !== sessionId) {
      throw new EngineProtocolError("bootstrap reply does not match its session-bound query")
    }
    const retained = retainedJsonBytes(event, maximum)
    if (retained > maximum) throw new EngineProtocolError("bootstrap retained payload exceeds its source-owned limit")
    allocation.resize(Math.max(allocation.bytes, retained))
    return event as Extract<EngineEvent, { type: Kind }>
  }
  try {
    tail = await collectLiveTail(async (target, request, signal, allocation) => {
      const reply = await post({ type: "read_transcript_tail", meta: meta(), session_id: target.sessionId, scope: target.scope, read: request }, signal, allocation)
      const event = reply.type === "read" ? reply.events[0] : undefined
      if (reply.outcome.type !== "accepted" || reply.type !== "read" || reply.events.length !== 1
        || event?.type !== "transcript_tail_ready" || event.session_id !== sessionId) throw new EngineProtocolError("bootstrap tail reply has an invalid source")
      return event.result
    }, cache, directSessionRead(sessionId), signal)
    const metadata = await read({ type: "get_session_state", meta: meta(), session_id: sessionId }, "session_state_ready", "metadata", MAX_SESSION_STATE_PREPARED_BYTES)
    if (tail.pages[0]?.identity.turn_started !== (metadata.snapshot.active_turn?.started ?? null)) throw new TailChanged("active turn changed during bootstrap")
    const controls = await read({ type: "get_session_controls", meta: meta(), session_id: sessionId }, "session_controls_ready", "controls", MAX_SESSION_CONTROLS_PREPARED_BYTES)
    const children = await read({ type: "read_session_children", meta: meta(), session_id: sessionId, scope: { type: "session" } }, "session_children_ready", "children", MAX_SESSION_CHILDREN_PREPARED_BYTES)
    const todos = await read({ type: "get_todos", meta: meta(), session_id: sessionId, scope: { type: "session" } }, "todos_read", "tasks", CLIENT_TASK_REPLY_BYTES)
    if (children.result.type !== "ready" || todos.result.type !== "ready") throw new TailChanged("session projection is catching up")
    let state = readSessionState(createInitialState(), sessionId, metadata.snapshot)
    state = installLiveTail(state, tail.pages)
    state = readControls(state, controls.snapshot)
    state = restoreChildren(state, children.result.snapshot)
    state = { ...state, todos: readTodos(state.todos, todos.result) }
    let through = state.recovery.tail?.through ?? null
    for (const source of [metadata.snapshot.through, controls.snapshot.through, children.result.snapshot.through, todos.result.todos.through]) through = minimumSequence(through, source)
    state = { ...state, lastSequence: through }
    return new SessionBootstrap(state, tail, allocations)
  } catch (error) {
    tail?.release()
    for (const allocation of allocations) allocation.release()
    throw error
  }
}
