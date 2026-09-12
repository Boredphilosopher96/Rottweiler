import type { FamilyControlsReader } from "../../src/family-controls-reader"
import type { SessionReadScope } from "../../src/protocol"

/** A retained live actor with an exact indexed binding; it has no pending controls in this fixture. */
export function retainedChildReader(root: string, subagent: string, child: string, spawned: string): FamilyControlsReader {
  return {
    watch: (_root, _after, signal) => new Promise((_, reject) => {
      if (signal.aborted) reject(signal.reason)
      else signal.addEventListener("abort", () => reject(signal.reason), { once: true })
    }),
    async child() { throw new Error("no selected control snapshot in history fixture") },
    async state() { throw new Error("no selected scalar snapshot in history fixture") },
    async scope(selectedRoot, target) {
      if (selectedRoot !== root || target.session_id !== child || target.ancestry.length !== 1
        || target.ancestry[0]?.session_id !== child || target.ancestry[0]?.subagent_id !== subagent) throw new Error("fixture child binding mismatch")
      const scope: SessionReadScope = { type: "descendant", root_session_id: root,
        ancestry: [{ subagent_id: subagent, session_id: child, source_sequence: spawned }] }
      return { type: "ready", scope }
    },
  }
}
