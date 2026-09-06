import { setTimeout as delay } from "node:timers/promises"
import type { ChildControlTarget } from "../../../protocol/types"
import { MAX_SESSION_CHILDREN_PREPARED_BYTES, MAX_FAMILY_CONTROLS_PREPARED_BYTES } from "../../../protocol/types"
import { ClientAllocationOwner } from "./client-allocation"
import { retainedJsonBytes } from "./retained-json"
import { descendantSessionRead, directSessionRead, type SessionReader, type SessionReadTarget } from "./session-reader"

/** Resolve every live binding through the canonical parent source before reading child history. */
export async function resolveFamilyHistory(reader: SessionReader, owner: ClientAllocationOwner, root: string, target: ChildControlTarget, signal: AbortSignal) {
  const targetBytes = retainedJsonBytes(target, MAX_FAMILY_CONTROLS_PREPARED_BYTES)
  const retained = owner.reserve("children", targetBytes)
  let source: SessionReadTarget = directSessionRead(root)
  try {
    const captured = structuredClone(target)
    for (const hop of captured.ancestry) {
      for (;;) {
        using page = owner.reserve("children", 0)
        const result = await reader.children(source, signal, { admit(bytes) {
          if (bytes > MAX_SESSION_CHILDREN_PREPARED_BYTES) throw new Error("Child association snapshot exceeds its prepared allowance.")
          page.admit(bytes)
        } })
        signal.throwIfAborted()
        if (result.type === "catching_up") { await delay(0, undefined, { signal }); continue }
        const child = result.snapshot.children.find(item => item.subagent_id === hop.subagent_id && item.child_session_id === hop.session_id)
        if (child === undefined) throw new Error("Child history binding is no longer active.")
        const next = descendantSessionRead(source, { subagent_id: child.subagent_id, session_id: child.child_session_id, source_sequence: child.spawned })
        const bytes = retainedJsonBytes(next, MAX_FAMILY_CONTROLS_PREPARED_BYTES)
        retained.resize(targetBytes + retainedJsonBytes(source, MAX_FAMILY_CONTROLS_PREPARED_BYTES) + bytes)
        source = structuredClone(next)
        retained.resize(targetBytes + bytes)
        break
      }
    }
    if (source.sessionId !== captured.session_id) throw new Error("Child history target does not match its ancestry.")
    return { target: source, release: () => retained.release() }
  } catch (error) { retained.release(); throw error }
}
