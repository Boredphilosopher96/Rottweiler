import type { SessionChildrenSnapshot } from "../../../../protocol/types"
import { MAX_ACTIVE_CHILDREN, MAX_CHILD_TASK_PREVIEW_BYTES, MAX_SESSION_CHILDREN_BYTES, MAX_SESSION_CHILDREN_PREPARED_BYTES } from "../../../../protocol/types"
import type { EngineEvent } from "../protocol"
import { jsonEncodedBytes } from "../json-size"
import { retainedJsonBytes } from "../retained-json"
import { EngineProtocolError } from "../transport/errors"
import type { RottweilerState, SubagentProjection } from "./model"

export interface ChildrenFence { readonly through: string | null; readonly observed: string | null }
export function emptyChildrenFence(): ChildrenFence { return { through: null, observed: null } }

/** Current active associations replace the live list; completed rows remain in semantic history. */
export function restoreChildren(state: RottweilerState, snapshot: SessionChildrenSnapshot): RottweilerState {
  const observed = state.recovery.children.observed
  if (observed !== null && (snapshot.through === null || BigInt(snapshot.through) < BigInt(observed))) return state
  if (snapshot.children.length > MAX_ACTIVE_CHILDREN
    || jsonEncodedBytes(snapshot, MAX_SESSION_CHILDREN_BYTES) > MAX_SESSION_CHILDREN_BYTES
    || retainedJsonBytes(snapshot, MAX_SESSION_CHILDREN_PREPARED_BYTES) > MAX_SESSION_CHILDREN_PREPARED_BYTES) {
    throw new EngineProtocolError("active children exceed their source-owned allocation bounds")
  }
  const subagents: Record<string, SubagentProjection> = Object.create(null)
  const order: string[] = []
  for (const child of snapshot.children) {
    if (subagents[child.subagent_id] !== undefined || snapshot.through === null
      || BigInt(child.spawned) > BigInt(snapshot.through) || Buffer.byteLength(child.task_preview) > MAX_CHILD_TASK_PREVIEW_BYTES) {
      throw new EngineProtocolError("active child association has an invalid source or identity")
    }
    const previous = state.subagents[child.subagent_id]
    const matching = previous?.childSessionId === child.child_session_id && previous.parentTurnId === child.spawned_turn
    subagents[child.subagent_id] = { projectionId: child.subagent_id, subagentId: child.subagent_id,
      parentTurnId: child.spawned_turn, task: child.task_preview, spawnedAtMs: matching ? previous.spawnedAtMs : null,
      status: "running", childSessionId: child.child_session_id, lastChildSequence: matching ? previous.lastChildSequence : null,
      activity: matching ? previous.activity : "working", summary: null, touchedFileCount: 0, diffArtifactId: null }
    order.push(child.subagent_id)
  }
  return { ...state, subagents, subagentOrder: order,
    recovery: { ...state.recovery, children: { through: snapshot.through, observed: snapshot.through } } }
}

/** Child lifecycle and parent metadata have independent committed source fences. */
export function childrenEvent(event: EngineEvent): boolean {
  return event.type === "subagent_spawned" || event.type === "subagent_finished" || event.type === "conversation_rewound"
}
export function coveredChildren(state: RottweilerState, event: EngineEvent, sequence: string): boolean {
  const cut = state.recovery.children.through
  return cut !== null && BigInt(sequence) <= BigInt(cut)
    && (childrenEvent(event) || event.type === "subagent_progress")
}
export function observeChildren(before: RottweilerState, after: RottweilerState, event: EngineEvent, sequence: string): RottweilerState {
  if (!childrenEvent(event)) return after
  if (coveredChildren(before, event, sequence)) return { ...after, subagents: before.subagents, subagentOrder: before.subagentOrder }
  return { ...after, recovery: { ...after.recovery, children: { ...after.recovery.children, observed: sequence } } }

}
