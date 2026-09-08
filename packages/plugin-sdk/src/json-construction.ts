import { isBigIntObject, isBooleanObject, isNumberObject, isStringObject } from "node:util/types"
import type { JsonValue } from "./generated/protocol-3"

// Bound our ancestor bookkeeping and reject graphs deeper than the host JSON
// decoder can accept. Native VM own-key enumeration and plugin getters/toJSON
// are not controlled by this encoded-output allowance.
export const MAX_JSON_CONSTRUCTION_DEPTH = 128

interface Parent {
  readonly value: object
  readonly array: boolean
  entries: number
}

/** Native JSON semantics with admission before each encoded contribution grows.
 * No full escaped string or UTF-8 buffer is produced just to measure its size.
 * Native own-key enumeration can use VM scratch before visiting child values;
 * this is an encoded-output and bounded-ancestor contract, not a heap limit.
 */
export function boundedJsonStringify(
  value: JsonValue,
  maxBytes: number,
  exceeded: () => Error,
): { readonly text: string; readonly bytes: number } {
  let bytes = 0
  let root = true
  const parents: Parent[] = []
  const add = (amount: number): void => {
    if (amount > maxBytes - bytes) throw exceeded()
    bytes += amount
  }
  const string = (text: string): void => {
    add(2)
    for (let index = 0; index < text.length; index += 1) {
      const code = text.charCodeAt(index)
      if (code === 0x22 || code === 0x5c || code === 8 || code === 9 || code === 10 || code === 12 || code === 13) add(2)
      else if (code < 0x20) add(6)
      else if (code >= 0xd800 && code <= 0xdbff) {
        const low = text.charCodeAt(index + 1)
        if (low >= 0xdc00 && low <= 0xdfff) { add(4); index += 1 }
        else add(6)
      } else if (code >= 0xdc00 && code <= 0xdfff) add(6)
      else add(code < 0x80 ? 1 : code < 0x800 ? 2 : 3)
    }
  }
  const text = JSON.stringify(value, function (key, original: unknown): unknown {
    // Native stringify invokes toJSON before its replacer. Unbox here so a
    // customized primitive conversion runs once, at the same semantic boundary.
    let current = original
    if (typeof current === "object" && current !== null) {
      if (isNumberObject(current)) current = Number(current)
      else if (isStringObject(current)) current = String(current)
      else if (isBooleanObject(current)) current = Boolean.prototype.valueOf.call(current)
      else if (isBigIntObject(current)) current = BigInt.prototype.valueOf.call(current)
    }
    if (!root) while (parents.length > 0 && parents.at(-1)?.value !== this) parents.pop()
    const parent = root ? undefined : parents.at(-1)
    root = false
    const omitted = current === undefined || typeof current === "function" || typeof current === "symbol"
    if (omitted && parent?.array !== true) return current
    if (parent !== undefined) {
      if (parent.entries > 0) add(1)
      parent.entries += 1
      if (!parent.array) { string(key); add(1) }
    }
    if (omitted || current === null) add(4)
    else if (typeof current === "string") string(current)
    else if (typeof current === "number") add(Number.isFinite(current) ? String(current).length : 4)
    else if (typeof current === "boolean") add(current ? 4 : 5)
    else if (typeof current === "object") {
      if (parents.length >= MAX_JSON_CONSTRUCTION_DEPTH) throw new RangeError("JSON-RPC output nesting exceeded")
      add(2)
      parents.push({ value: current, array: Array.isArray(current), entries: 0 })
    }
    // BigInt/cycles and other native serialization errors retain native behavior.
    return current
  })
  if (text === undefined) throw new TypeError("JSON-RPC value is not serializable")
  return { text, bytes }
}
