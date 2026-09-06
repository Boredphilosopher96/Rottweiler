import { emptyChildrenFence, type ChildrenFence } from "./children-recovery"
import { restoreCompaction, type CompactionFence } from "./compaction-recovery"
import type { EngineEvent, SessionStateSnapshot, TranscriptTailIdentity } from "../protocol"
import { MAX_SESSION_STATE_BYTES, MAX_SESSION_STATE_PREPARED_BYTES } from "../../../../protocol/types"
import { jsonEncodedBytes } from "../json-size"
import { retainedJsonBytes } from "../retained-json"
import { EngineProtocolError } from "../transport/errors"
import { parseU64 } from "../transport/types"
import type { RottweilerState } from "./model"
import { UNKNOWN_ACTIVITY_TIMING } from "./tool-state"

export interface TailReplayFence {
  readonly identity: TranscriptTailIdentity
  readonly through: string | null
  readonly textThrough: string | null
  readonly thinkingThrough: string | null
  readonly citationsThrough: string | null
  readonly toolsThrough: string | null
  readonly invocations: Readonly<Record<string, string>>
}
export interface RecoveryProjection {
  readonly children: ChildrenFence
  readonly compaction: CompactionFence | null
  readonly metadataThrough: string | null
  readonly metadataObservedThrough: string | null
  readonly activeTurnSource: string | null
  readonly tail: TailReplayFence | null
}
export function emptyRecovery(): RecoveryProjection {
  return { children: emptyChildrenFence(), compaction: null, metadataThrough: null, metadataObservedThrough: null, activeTurnSource: null, tail: null }
}

/** A scalar snapshot does not authorize skipping durable display or interaction state. */
export function readSessionState(state: RottweilerState, sessionId: string, snapshot: SessionStateSnapshot): RottweilerState {
  const through = parseU64(snapshot.through), observed = parseU64(state.recovery.metadataObservedThrough)
  if (observed !== null && (through === null || through < observed)) return state
  if (jsonEncodedBytes(snapshot, MAX_SESSION_STATE_BYTES) > MAX_SESSION_STATE_BYTES
    || retainedJsonBytes(snapshot, MAX_SESSION_STATE_PREPARED_BYTES) > MAX_SESSION_STATE_PREPARED_BYTES) {
    throw new EngineProtocolError("session metadata exceeds its source-owned allocation limit")
  }
  const turns: Record<string, RottweilerState["turns"][string]> = Object.create(null)
  if (snapshot.active_turn !== null) turns[snapshot.active_turn.turn_id] = {
    turnId: snapshot.active_turn.turn_id, status: "running", usage: null, cost: null, timing: UNKNOWN_ACTIVITY_TIMING,
  }
  const restored = restoreCompaction(state, snapshot.compaction, snapshot.through)
  return {
    ...state, mode: snapshot.mode_id, model: snapshot.model_alias, provider: snapshot.provider,
    driverClientId: snapshot.driver_client_id, turns,
    sessions: state.sessions.map(session => session.sessionId === sessionId
      ? { ...session, ...(snapshot.title === null ? {} : { title: snapshot.title }), model: snapshot.model_alias, driverClientId: snapshot.driver_client_id, shellActive: snapshot.shell !== null }
      : session),
    hasActivity: snapshot.completed_turns !== "0" || snapshot.active_turn !== null,
    queuedMessages: snapshot.queued_messages.map(message => ({ position: message.position, content: message.preview })),
    shell: { shellId: snapshot.shell?.shell_id ?? null, active: snapshot.shell !== null, status: null, capturedOutput: null },
    latestShell: snapshot.shell === null ? null : {
      shellId: snapshot.shell.shell_id, command: snapshot.shell.command_preview, active: true, status: null,
      capturedOutput: "", outputTruncated: snapshot.shell.truncated,
    },
    compaction: restored.compaction,
    budgets: snapshot.budget === null ? [] : [{
      turnId: snapshot.budget.turn_id, level: snapshot.budget.level, scope: snapshot.budget.scope,
      unit: snapshot.budget.unit, current: snapshot.budget.current, limit: snapshot.budget.limit,
    }],
    recovery: { ...restored.recovery, metadataThrough: snapshot.through, metadataObservedThrough: snapshot.through,
      activeTurnSource: snapshot.active_turn?.started ?? null },
  }
}

export function metadataEvent(event: EngineEvent): boolean {
  switch (event.type) {
    case "session_created": case "driver_changed": case "session_title_updated": case "model_changed":
    case "mode_changed": case "user_shell_state_changed": case "message_queued": case "queued_message_removed":
    case "queued_messages_cleared": case "budget_status_changed": case "turn_started": case "turn_finished":
    case "conversation_turn_committed": case "conversation_rewound": case "compaction_started": case "compaction_finished": case "compaction_failed": return true
    default: return false
  }
}

export function preserveMetadata(before: RottweilerState, after: RottweilerState, event: EngineEvent, sequence: string): RottweilerState {
  if (!metadataEvent(event)) return after
  const cut = parseU64(before.recovery.metadataThrough)
  if (cut !== null && BigInt(sequence) <= cut) return {
    ...after, mode: before.mode, model: before.model, provider: before.provider, driverClientId: before.driverClientId,
    sessions: before.sessions, turns: before.turns, queuedMessages: before.queuedMessages, shell: before.shell,
    latestShell: before.latestShell, compaction: before.compaction, budgets: before.budgets,
    recovery: { ...after.recovery, compaction: before.recovery.compaction },
  }
  return { ...after, recovery: { ...after.recovery, metadataObservedThrough: sequence,
    activeTurnSource: event.type === "turn_started" ? sequence : (event.type === "turn_finished" || event.type === "conversation_rewound") ? null : after.recovery.activeTurnSource,
  } }
}
