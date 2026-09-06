import {
  ENGINE_EVENT_DELIVERY,
  type Attachment,
  type EngineEvent,
} from "./protocol"
import {
  createInitialState,
  type RottweilerState,
} from "./state"
import { isWireEngineEvent } from "./transport"
import { MAX_CHILD_TASK_PREVIEW_BYTES } from "../../../protocol/types"
import { utf8Prefix } from "./state/display-buffer"
import { boundedUiText } from "./ui-presentation"

const MAX_SUBAGENT_ID_LENGTH = 256
const MAX_CHILD_PROJECTION_ENTRIES = 512

export interface ComposerDraft {
  readonly content: string
  readonly attachments: readonly Attachment[]
}

export type SubagentDescriptor =
  Extract<EngineEvent, { type: "subagents_listed" }>["subagents"][number]

export function initialSubagentState(
  parent: RottweilerState,
  descriptor: SubagentDescriptor,
): RottweilerState {
  const state = createInitialState()
  return {
    ...state,
    connection: { ...state.connection, phase: "connected" },
    mode: parent.mode ?? "execute",
    model: descriptor.model,
  }
}

export function sanitizeSubagentDescriptor(
  descriptor: SubagentDescriptor,
): SubagentDescriptor | null {
  if (
    !safeSubagentIdentifier(descriptor.subagent_id) ||
    !safeSubagentIdentifier(descriptor.child_session_id)
  ) return null
  const task = boundedUiText(utf8Prefix(descriptor.task, MAX_CHILD_TASK_PREVIEW_BYTES), 512)
  return {
    subagent_id: descriptor.subagent_id, child_session_id: descriptor.child_session_id,
    isolation: descriptor.isolation, activity: descriptor.activity,
    task: task.length === 0 ? "Untitled child agent" : task,
    agent: boundedUiText(utf8Prefix(descriptor.agent, MAX_CHILD_TASK_PREVIEW_BYTES), 128),
    model: boundedUiText(utf8Prefix(descriptor.model, MAX_CHILD_TASK_PREVIEW_BYTES), 256),
  }
}

function safeSubagentIdentifier(value: string): boolean {
  return value.length > 0 &&
    value.length <= MAX_SUBAGENT_ID_LENGTH &&
    !/[\u0000-\u001f\u007f]/.test(value)
}

export function childEngineEvent(
  value: unknown,
  expectedSessionId: string,
): EngineEvent | null {
  if (!isWireEngineEvent(value)) return null
  const delivery: Readonly<Record<string, string>> = ENGINE_EVENT_DELIVERY
  const session = delivery[value.type] === "transient" && "session_id" in value ? value.session_id
    : "meta" in value && "session_id" in value.meta ? value.meta.session_id : undefined
  return session === expectedSessionId ? value : null
}

export function boundSubagentState(state: RottweilerState): RottweilerState {
  return {
    ...state,
    latestShell: null,
    turns: boundProjectionRecord(state.turns),
    tools: boundProjectionRecord(state.tools),
    commandAcks: boundProjectionRecord(state.commandAcks),
  }
}

function boundProjectionRecord<T>(
  record: Readonly<Record<string, T>>,
): Readonly<Record<string, T>> {
  const entries = Object.entries(record)
  return entries.length <= MAX_CHILD_PROJECTION_ENTRIES
    ? record
    : Object.fromEntries(entries.slice(-MAX_CHILD_PROJECTION_ENTRIES))
}

export function childPassiveInteractionState(state: RottweilerState): RottweilerState {
  return {
    ...state,
    tools: Object.fromEntries(
      Object.entries(state.tools).filter(([, tool]) => tool.status !== "awaiting_approval"),
    ),
    questions: {},
    pendingPlan: null,
  }
}
