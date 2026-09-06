import { setTimeout as delay } from "node:timers/promises"
import type { ChildControlTarget } from "../../../protocol/types"
import { MAX_FAMILY_CONTROLS_PREPARED_BYTES } from "../../../protocol/types"
import { ClientAllocationError, ClientAllocationOwner } from "./client-allocation"
import type { FamilyControlsReader } from "./family-controls-reader"
import { retainedJsonBytes } from "./retained-json"
import type { SessionReadTarget } from "./session-reader"

/** The live binding resolver includes retained terminal children without scanning historical lists. */
export async function resolveFamilyHistory(reader: Pick<FamilyControlsReader, "scope">, owner: ClientAllocationOwner, root: string, target: ChildControlTarget, signal: AbortSignal) {
  const targetBytes = retainedJsonBytes(target, MAX_FAMILY_CONTROLS_PREPARED_BYTES)
  if (targetBytes > MAX_FAMILY_CONTROLS_PREPARED_BYTES) throw new ClientAllocationError("child history target exceeds its allowance")
  const retained = owner.reserve("children", targetBytes)
  try {
    const captured = structuredClone(target)
    for (;;) {
      using page = owner.reserve("children", 0)
      const result = await reader.scope(root, captured, signal, { admit(bytes) {
        if (bytes > MAX_FAMILY_CONTROLS_PREPARED_BYTES) throw new ClientAllocationError("child history scope exceeds its prepared allowance")
        page.admit(bytes)
      } })
      signal.throwIfAborted()
      if (result.type === "catching_up") { await delay(0, undefined, { signal }); continue }
      const scope = result.scope
      if (scope.type !== "descendant" || scope.root_session_id !== root || scope.ancestry.length !== captured.ancestry.length
        || scope.ancestry.at(-1)?.session_id !== captured.session_id
        || scope.ancestry.some((hop, index) => hop.subagent_id !== captured.ancestry[index]?.subagent_id || hop.session_id !== captured.ancestry[index]?.session_id)) {
        throw new Error("Child history scope does not match its live ancestry.")
      }
      const source: SessionReadTarget = { sessionId: captured.session_id, scope }
      const bytes = retainedJsonBytes(source, MAX_FAMILY_CONTROLS_PREPARED_BYTES)
      if (bytes > MAX_FAMILY_CONTROLS_PREPARED_BYTES) throw new ClientAllocationError("child history scope exceeds its retained allowance")
      retained.resize(targetBytes + bytes)
      return { target: structuredClone(source), release: () => retained.release() }
    }
  } catch (error) { retained.release(); throw error }
}
