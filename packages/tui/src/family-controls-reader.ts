import type { ChildReadScopeResult, SessionStateSnapshot, ChildControlTarget, ChildControlsSnapshot, FamilyControlsSnapshot, CommandMeta } from "../../../protocol/types"
import type { ClientCommand, CommandReply } from "./protocol"
import type { ReplyAllocation } from "./transport/reply-allocation"

export interface FamilyControlsReader {
  state(root: string, target: ChildControlTarget, signal: AbortSignal, allocation: ReplyAllocation): Promise<SessionStateSnapshot>
  scope(root: string, target: ChildControlTarget, signal: AbortSignal, allocation: ReplyAllocation): Promise<ChildReadScopeResult>
  watch(root: string, after: string | null, signal: AbortSignal, allocation: ReplyAllocation): Promise<FamilyControlsSnapshot>
  child(root: string, target: ChildControlTarget, signal: AbortSignal, allocation: ReplyAllocation): Promise<ChildControlsSnapshot>
}
export type FamilyReadCommand = Extract<ClientCommand, { type: "read_family_controls" | "read_child_controls" | "read_child_state" | "resolve_child_read_scope" }>

export function sameChildTarget(left: ChildControlTarget, right: ChildControlTarget): boolean {
  return left.session_id === right.session_id && left.ancestry.length === right.ancestry.length
    && left.ancestry.every((hop, index) => hop.subagent_id === right.ancestry[index]?.subagent_id && hop.session_id === right.ancestry[index]?.session_id)
}

/** Family control authority is live and distinct from canonical transcript read scopes. */
export function familyControlsReader(
  read: (command: FamilyReadCommand, signal: AbortSignal, allocation: ReplyAllocation) => Promise<Extract<CommandReply, { type: "read" }>>,
  meta: () => CommandMeta,
): FamilyControlsReader {
  return {
    async state(root, target, signal, allocation) {
      const reply = await read({ type: "read_child_state", meta: meta(), session_id: root, target }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "child_state_ready" || event.session_id !== root || !sameChildTarget(event.target, target)) throw new Error("child state reply has no target-bound snapshot")
      return event.snapshot
    },
    async scope(root, target, signal, allocation) {
      const reply = await read({ type: "resolve_child_read_scope", meta: meta(), session_id: root, target }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "child_read_scope_ready" || event.session_id !== root || !sameChildTarget(event.target, target)) throw new Error("child history reply has no target-bound scope")
      return event.result
    },
    async watch(root, after_revision, signal, allocation) {
      const reply = await read({ type: "read_family_controls", meta: meta(), session_id: root, after_revision }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "family_controls_ready" || event.session_id !== root) throw new Error("family controls reply has no root-bound snapshot")
      return event.snapshot
    },
    async child(root, target, signal, allocation) {
      const reply = await read({ type: "read_child_controls", meta: meta(), session_id: root, target }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "child_controls_ready" || event.session_id !== root || !sameChildTarget(event.target, target)) throw new Error("child controls reply has no target-bound snapshot")
      return event.snapshot
    },
  }
}
