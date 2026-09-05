import type { EngineEvent, TranscriptTailPage } from "../protocol"
import { EngineProtocolError } from "../transport/errors"
import { sameTailIdentity } from "../history/live-tail"
import { ToolOutputBuffer } from "./display-buffer"
import { createStreamingTail, type RottweilerState, type ToolProjection } from "./model"
import { UNKNOWN_ACTIVITY_TIMING } from "./tool-state"
import type { TailReplayFence } from "./recovery"

export function minimumSequence(left: string | null, right: string | null): string | null {
  return left === null || right === null ? null : BigInt(left) <= BigInt(right) ? left : right
}

/** References remain owned by the caller's page leases until this state leaves presentation. */
export function installLiveTail(state: RottweilerState, pages: readonly TranscriptTailPage[]): RottweilerState {
  const text = pages.find(page => page.content.type === "text")
  const thinking = pages.find(page => page.content.type === "thinking")
  if (text?.content.type !== "text" || thinking?.content.type !== "thinking"
    || !pages.some(page => page.content.type === "citations") || !pages.some(page => page.content.type === "tools")) {
    throw new EngineProtocolError("live tail recovery is missing a display component")
  }
  const identity = text.identity
  if (identity.turn_started !== state.recovery.activeTurnSource || pages.some(page => !sameTailIdentity(identity, page.identity))) {
    throw new EngineProtocolError("live tail recovery does not match the active turn source")
  }
  const tools: Record<string, ToolProjection> = Object.create(null)
  const invocations: Record<string, string> = Object.create(null)
  const citations: { uri: string; title: string | null }[] = []
  let through = text.view.through
  let citationsThrough: string | null = null
  let toolsThrough: string | null | undefined
  for (const page of pages) {
    through = minimumSequence(through, page.view.through)
    if (page.content.type === "citations") {
      citationsThrough = page.view.through
      for (const citation of page.content.items) citations.push({ uri: citation.uri, title: citation.title })
    }
    if (page.content.type !== "tools") continue
    toolsThrough = toolsThrough === undefined ? page.view.through : minimumSequence(toolsThrough, page.view.through)
    for (const item of page.content.items) {
      if (page.view.through === null || Object.hasOwn(tools, item.invocation_id)) throw new EngineProtocolError("live tail repeated an invocation source")
      invocations[item.invocation_id] = page.view.through
      tools[item.invocation_id] = {
        invocationId: item.invocation_id, toolCallId: item.tool_call_id, turnId: item.turn_id, name: item.name,
        // Full arguments and diffs belong to their canonical sources or the independent approval snapshot.
        args: null, status: "running", capabilities: [], rationale: null, diffSource: item.diff?.source ?? null, diff: null,
        chunks: ToolOutputBuffer.fromPreview(item.output.text, item.output.truncated), display: null, source: null,
        isError: null, callIndex: item.call_index, timing: UNKNOWN_ACTIVITY_TIMING,
      }
    }
  }
  const turnId = Object.values(state.turns).find(turn => turn.status === "running")?.turnId
  if (turnId === undefined && (text.content.preview.text !== "" || thinking.content.preview.text !== "" || citations.length !== 0)) {
    throw new EngineProtocolError("live tail has display text without an active turn")
  }
  const streamingTail = turnId === undefined ? null : createStreamingTail({ turnId, text: text.content.preview.text,
    thinking: thinking.content.preview.text, citations, toolInvocationIds: Object.keys(tools), finished: null })
  const fence: TailReplayFence = { identity, through, textThrough: text.view.through, thinkingThrough: thinking.view.through,
    citationsThrough, toolsThrough: toolsThrough ?? null, invocations }
  return { ...state, tools, streamingTail: streamingTail === null ? null : {
    ...streamingTail, displayBudget: {
      text: { ...streamingTail.displayBudget.text, omittedBytes: text.content.preview.truncated ? 1 : 0 },
      thinking: { ...streamingTail.displayBudget.thinking, omittedBytes: thinking.content.preview.truncated ? 1 : 0 },
    },
  }, recovery: { ...state.recovery, tail: fence } }
}

function covered(sequence: string, through: string | null): boolean {
  return through !== null && BigInt(sequence) <= BigInt(through)
}

/** Skip only the component whose exact source prefix was installed; always advance the transport cursor. */
export function coveredTailDelta(state: RottweilerState, event: EngineEvent, sequence: string): boolean {
  const fence = state.recovery.tail
  if (fence === null) return false
  switch (event.type) {
    case "text_delta": return covered(sequence, fence.textThrough)
    case "thinking_delta": return covered(sequence, fence.thinkingThrough)
    case "citation_delta": return covered(sequence, fence.citationsThrough)
    case "tool_call_started": case "tool_call_finished": case "tool_output_delta": case "tool_diff_ready":
      return covered(sequence, fence.toolsThrough) || covered(sequence, fence.invocations[event.invocation_id] ?? null)
    default: return false
  }
}

export function preserveTail(before: RottweilerState, after: RottweilerState, event: EngineEvent, sequence: string): RottweilerState {
  const fence = before.recovery.tail
  if (fence === null) return after
  switch (event.type) {
    case "conversation_turn_committed": case "conversation_rewound": case "turn_started": case "turn_finished":
    case "compaction_started": case "compaction_finished": case "compaction_failed":
      return covered(sequence, fence.through) ? { ...after, tools: before.tools, streamingTail: before.streamingTail } : after
    default: return after
  }
}
