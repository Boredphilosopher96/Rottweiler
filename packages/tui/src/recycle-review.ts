import { interactionFingerprint } from "./interaction-selection"
import type { RottweilerState } from "./state"

/** A view hint is usable only beside a newly read, identical source. */
export interface RecycleReview {
  readonly mode: "session" | "workspace"
  readonly path: string
  readonly fingerprint: string
  readonly roots: string
  readonly scrollTop: number
  readonly scrollLeft: number
}
export function reviewRoots(state: RottweilerState): string {
  return interactionFingerprint(state.workspaceRoots)
}
export function reviewFingerprint(value: unknown): string { return interactionFingerprint(value) }
export function parseRecycleReview(value: unknown): RecycleReview | null {
  if (typeof value !== "object" || value === null) return null
  const item = value as Record<string, unknown>
  if (Object.keys(item).length !== 6 || (item.mode !== "session" && item.mode !== "workspace")
    || typeof item.path !== "string" || item.path.length === 0 || Buffer.byteLength(item.path) > 4096
    || typeof item.fingerprint !== "string" || !/^[0-9a-f]{64}$/.test(item.fingerprint)
    || typeof item.roots !== "string" || !/^[0-9a-f]{64}$/.test(item.roots)
    || !offset(item.scrollTop) || !offset(item.scrollLeft)) return null
  return { mode: item.mode, path: item.path, fingerprint: item.fingerprint, roots: item.roots,
    scrollTop: item.scrollTop, scrollLeft: item.scrollLeft }
}
function offset(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 && value <= 16_777_216
}
