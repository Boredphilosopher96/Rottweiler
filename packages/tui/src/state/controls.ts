import type { EngineEvent, SessionControlsSnapshot } from "../protocol"
import { MAX_SESSION_CONTROLS_BYTES, MAX_SESSION_CONTROLS_PREPARED_BYTES, MAX_PENDING_QUESTION_REQUESTS, MAX_QUESTION_SET_BYTES, MAX_PENDING_TOOL_INVOCATIONS, MAX_PENDING_PLAN_BYTES } from "../../../../protocol/types"
import { jsonEncodedBytes } from "../json-size"
import { retainedJsonBytes } from "../retained-json"
import { EngineProtocolError } from "../transport/errors"
import { parseU64 } from "../transport/types"
import { EMPTY_TOOL_OUTPUT } from "./display-buffer"
import type { RottweilerState, ToolProjection } from "./model"
import { UNKNOWN_ACTIVITY_TIMING } from "./tool-state"

export interface ControlFence {
  readonly snapshotThrough: string | null
  readonly observedThrough: string | null
}
export function emptyControlFence(): ControlFence { return { snapshotThrough: null, observedThrough: null } }

/** Source-owned controls replace only the live interaction projection, never the replay cursor. */
export function readControls(state: RottweilerState, snapshot: SessionControlsSnapshot): RottweilerState {
  const through = parseU64(snapshot.through)
  const observed = parseU64(state.controls.observedThrough)
  if (observed !== null && (through === null || through < observed)) return state
  if (jsonEncodedBytes(snapshot, MAX_SESSION_CONTROLS_BYTES) > MAX_SESSION_CONTROLS_BYTES
    || retainedJsonBytes(snapshot, MAX_SESSION_CONTROLS_PREPARED_BYTES) > MAX_SESSION_CONTROLS_PREPARED_BYTES) {
    throw new EngineProtocolError("session controls exceed the source-owned admission limit")
  }
  if (snapshot.controls.questions.length > MAX_PENDING_QUESTION_REQUESTS
    || snapshot.controls.approvals.length > MAX_PENDING_TOOL_INVOCATIONS
    || (snapshot.controls.pending_plan !== null && jsonEncodedBytes(snapshot.controls.pending_plan, MAX_PENDING_PLAN_BYTES) > MAX_PENDING_PLAN_BYTES)) {
    throw new EngineProtocolError("session control entries exceed the source-owned admission limit")
  }
  const questions: Record<string, RottweilerState["questions"][string]> = Object.create(null)
  for (const question of snapshot.controls.questions) {
    if (question.questions.length === 0 || question.questions.length > MAX_PENDING_QUESTION_REQUESTS
      || jsonEncodedBytes(question.questions, MAX_QUESTION_SET_BYTES) > MAX_QUESTION_SET_BYTES) {
      throw new EngineProtocolError("session question exceeds the source-owned admission limit")
    }
    if (Object.hasOwn(questions, question.question_id)) throw new EngineProtocolError("duplicate session question identity")
    questions[question.question_id] = { questionId: question.question_id, turnId: question.turn_id, questions: question.questions }
  }
  const approvals = new Map(snapshot.controls.approvals.map(approval => [approval.invocation_id, approval]))
  if (approvals.size !== snapshot.controls.approvals.length) throw new EngineProtocolError("duplicate session approval identity")
  const tools: Record<string, ToolProjection> = Object.assign(Object.create(null), state.tools)
  for (const [id, tool] of Object.entries(tools)) {
    if (tool.status === "awaiting_approval" && !approvals.has(id)) tools[id] = resolvedApproval(tool)
  }
  for (const [id, approval] of approvals) {
    const existing = tools[id]
    if (existing !== undefined && (existing.toolCallId !== approval.tool_call_id || existing.turnId !== approval.turn_id)) {
      throw new EngineProtocolError("session approval conflicts with its invocation identity")
    }
    tools[id] = {
      toolCallId: approval.tool_call_id, invocationId: id, turnId: approval.turn_id,
      name: approval.name, args: approval.args, status: "awaiting_approval",
      capabilities: approval.capabilities, rationale: approval.rationale, diff: approval.diff,
      diffSource: existing?.diffSource ?? null, chunks: existing?.chunks ?? EMPTY_TOOL_OUTPUT,
      display: existing?.display ?? null, source: existing?.source ?? null,
      isError: existing?.isError ?? null, callIndex: existing?.callIndex ?? 0,
      timing: existing?.timing ?? UNKNOWN_ACTIVITY_TIMING,
    }
  }
  return { ...state, questions, tools, pendingPlan: snapshot.controls.pending_plan,
    controls: { snapshotThrough: snapshot.through, observedThrough: snapshot.through } }
}

export function resolvedApproval(tool: ToolProjection): ToolProjection {
  return { ...tool, status: "running", capabilities: [], rationale: null }
}

export function isControlEvent(event: EngineEvent): boolean {
  switch (event.type) {
    case "question_asked": case "question_answered": case "tool_approval_needed": case "tool_approval_resolved":
    case "tool_call_finished": case "plan_submitted": case "plan_reviewed": case "mode_changed": return true
    default: return false
  }
}

export function coveredByControlSnapshot(state: RottweilerState, sequence: string): boolean {
  const through = parseU64(state.controls.snapshotThrough)
  return through !== null && BigInt(sequence) <= through
}

/** Replay still builds transcript/tool activity, but cannot resurrect controls covered by a snapshot. */
export function preserveSnapshotControls(before: RottweilerState, after: RottweilerState, event: EngineEvent): RottweilerState {
  let tools = after.tools
  if ("invocation_id" in event) {
    const previous = before.tools[event.invocation_id]
    const next = after.tools[event.invocation_id]
    if (previous?.status === "awaiting_approval") tools = { ...tools, [event.invocation_id]: previous }
    else if (next?.status === "awaiting_approval") tools = { ...tools, [event.invocation_id]: resolvedApproval(next) }
  }
  return { ...after, tools, questions: before.questions, pendingPlan: before.pendingPlan }
}
