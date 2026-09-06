import { MAX_SESSION_READ_ANCESTORS } from "./protocol"
import type { ChildControlTarget } from "../../../protocol/types"
import { descendantSessionRead, directSessionRead, type SessionReadTarget } from "./session-reader"
import { parseU64 } from "./transport/types"

export type RecycleChildTarget =
  | { readonly type: "live"; readonly target: ChildControlTarget }
  | { readonly type: "historical"; readonly target: SessionReadTarget }

const record = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value)
const id = (value: unknown): value is string => typeof value === "string" && value.length > 0 && value.length <= 1024

/** Saved identities select fresh reads; they do not grant child mutation authority. */
export function parseRecycleChild(value: unknown, root: string): RecycleChildTarget | null {
  if (!record(value) || !record(value.target)) return null
  if (value.type === "live") {
    const raw = value.target
    if (!id(raw.session_id) || !Array.isArray(raw.ancestry) || raw.ancestry.length < 1 || raw.ancestry.length > MAX_SESSION_READ_ANCESTORS) return null
    const ancestry: ChildControlTarget["ancestry"] = [], seen = new Set([root])
    for (const hop of raw.ancestry) {
      if (!record(hop) || !id(hop.subagent_id) || !id(hop.session_id) || seen.has(hop.session_id)) return null
      seen.add(hop.session_id); ancestry.push({ subagent_id: hop.subagent_id, session_id: hop.session_id })
    }
    return ancestry.at(-1)?.session_id === raw.session_id ? { type: "live", target: { session_id: raw.session_id, ancestry } } : null
  }
  if (value.type !== "historical" || !id(value.target.sessionId) || !record(value.target.scope)) return null
  const scope = value.target.scope
  if (scope.type !== "descendant" || scope.root_session_id !== root || !Array.isArray(scope.ancestry)
    || scope.ancestry.length < 1 || scope.ancestry.length > MAX_SESSION_READ_ANCESTORS) return null
  let target = directSessionRead(root)
  try {
    for (const hop of scope.ancestry) {
      if (!record(hop) || !id(hop.subagent_id) || !id(hop.session_id) || typeof hop.source_sequence !== "string" || parseU64(hop.source_sequence) === null) return null
      target = descendantSessionRead(target, { subagent_id: hop.subagent_id, session_id: hop.session_id, source_sequence: hop.source_sequence })
    }
  } catch { return null }
  return target.sessionId === value.target.sessionId ? { type: "historical", target } : null
}
