import { createHash } from "node:crypto"

/** A local highlight is usable only beside the same authoritative control payload. */
export interface InteractionSelection {
  readonly fingerprint: string
  readonly index: number
}

export function parseInteractionSelection(value: unknown): InteractionSelection | null {
  if (typeof value !== "object" || value === null) return null
  const item = value as Record<string, unknown>
  return typeof item.fingerprint === "string" && /^[0-9a-f]{64}$/.test(item.fingerprint)
    && typeof item.index === "number" && Number.isSafeInteger(item.index) && item.index >= 0 && item.index <= 65536
    ? { fingerprint: item.fingerprint, index: item.index } : null
}

/** Incremental framing avoids allocating a second encoded approval diff or question body. */
export function interactionFingerprint(value: unknown): string {
  const hash = createHash("sha256")
  const visit = (item: unknown, depth: number): void => {
    if (depth > 64) throw new RangeError("interaction payload nesting exceeds its bound")
    if (typeof item === "string") { hash.update(`s${item.length}:`); hash.update(item, "utf16le"); return }
    if (item === null || item === undefined || typeof item !== "object") { hash.update(`${typeof item}:${String(item)};`); return }
    if (Array.isArray(item)) { hash.update("["); for (const child of item) visit(child, depth + 1); hash.update("]"); return }
    hash.update("{")
    for (const key in item) {
      if (!Object.hasOwn(item, key)) continue
      visit(key, depth + 1); visit(Reflect.get(item, key), depth + 1)
    }
    hash.update("}")
  }
  visit(value, 0)
  return hash.digest("hex")
}
