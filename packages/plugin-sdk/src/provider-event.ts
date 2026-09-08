import validateProviderEvent, { PROVIDER_EVENT_FIELDS } from "./generated/provider-event-validator.js"
import type { ProviderEvent } from "./generated/provider-contract"

const fields = new Set(PROVIDER_EVENT_FIELDS)

/** One schema-bounded own-field snapshot supplies validation, classification and
 * output. Nested JSON values remain references with their native JSON semantics.
 * VM enumeration scratch and allocations made by authored getters are outside
 * this fixed top-level record allowance, as with bounded JSON construction.
 */
export function captureProviderEvent(value: unknown): ProviderEvent | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined
  const prototype: unknown = Object.getPrototypeOf(value)
  if (prototype !== null && prototype !== Object.prototype) return undefined
  const captured: Record<string, unknown> = Object.create(null)
  // Do not build an unbounded Object.keys/descriptors array or copy unknown
  // fields. The generated schema's finite key set bounds our record growth.
  for (const key in value) {
    if (!Object.hasOwn(value, key) || !fields.has(key)) return undefined
    captured[key] = (value as Record<string, unknown>)[key]
  }
  return validateProviderEvent(captured) ? Object.freeze(captured) : undefined
}
