import type Ajv2020 from "ajv/dist/2020"
import { _Code } from "ajv/dist/compile/codegen/code"
import { _ } from "ajv/dist/compile/codegen"

interface PairRule {
  left: string[]
  right: string[]
  identity: string
  discriminator: string
  maxItems: number
  collections: { tag: string; values: string; limit: string; width?: string }[]
}

/** Fixed-path, bounded positional correspondence. Structural schemas own values. */
function arraysCorrespond(data: unknown, rule: PairRule): boolean {
  const object = (value: unknown): value is Record<string, unknown> =>
    typeof value === "object" && value !== null && !Array.isArray(value)
  const select = (path: string[]): unknown => {
    let value = data
    for (const field of path) {
      if (!object(value) || !Object.hasOwn(value, field)) return undefined
      value = value[field]
    }
    return value
  }
  const left = select(rule.left), right = select(rule.right)
  if (!Array.isArray(left) || !Array.isArray(right)
    || left.length > rule.maxItems || left.length !== right.length) return false
  const identities = new Set<string>()
  for (let i = 0; i < left.length; i++) {
    const declaration: unknown = left[i], projected: unknown = right[i]
    if (!object(declaration) || !object(projected)) return false
    const id = declaration[rule.identity], tag = declaration[rule.discriminator]
    if (typeof id !== "string" || typeof tag !== "string" || identities.has(id)
      || projected[rule.identity] !== id || projected[rule.discriminator] !== tag) return false
    identities.add(id)
    for (const collection of rule.collections) {
      if (collection.tag !== tag) continue
      const values = projected[collection.values], limit = declaration[collection.limit]
      if (!Array.isArray(values) || typeof limit !== "number" || !Number.isSafeInteger(limit)
        || limit < 0 || limit > rule.maxItems || values.length > limit) return false
      if (collection.width !== undefined) {
        const columns = declaration[collection.width]
        if (!Array.isArray(columns) || columns.length > rule.maxItems) return false
        for (const row of values) if (!Array.isArray(row) || row.length !== columns.length) return false
      }
    }
  }
  return true
}

export function addArrayPair(ajv: Ajv2020): void {
  const field = { type: "string", minLength: 1, maxLength: 128 }
  const path = { type: "array", minItems: 1, maxItems: 8, items: field }
  ajv.addKeyword({
    keyword: "x-rw-array-pair", type: "object", schemaType: "object",
    metaSchema: {
      type: "object", additionalProperties: false,
      required: ["left", "right", "identity", "discriminator", "maxItems", "collections"],
      properties: {
        left: path, right: path, identity: field, discriminator: field,
        maxItems: { type: "integer", minimum: 0, maximum: 128 },
        collections: { type: "array", maxItems: 8, items: {
          type: "object", additionalProperties: false, required: ["tag", "values", "limit"],
          properties: { tag: field, values: field, limit: field, width: field },
        } },
      },
    },
    code(ctx) {
      const validate = ctx.gen.scopeValue("func", {
        ref: arraysCorrespond, code: new _Code(arraysCorrespond.toString()),
      })
      ctx.fail(_`!${validate}(${ctx.data}, ${ctx.schema})`)
    },
  })
}
