import { jsonEncodedBytes } from "../src/json-size"
import { _Code } from "ajv/dist/compile/codegen/code"
import type Ajv2020 from "ajv/dist/2020"
import { _, type Name } from "ajv/dist/compile/codegen"
import type { KeywordCxt } from "ajv"

/** Bounded UTF-8 measurement without constructing an encoded copy. */
function utf8Bytes(value: string, limit: number): number {
  let bytes = 0
  for (let i = 0; i < value.length; i++) {
    const code = value.charCodeAt(i)
    if (code < 128) bytes++
    else if (code < 2048) bytes += 2
    else if (code >= 55296 && code <= 56319 && i + 1 < value.length
      && value.charCodeAt(i + 1) >= 56320 && value.charCodeAt(i + 1) <= 57343) { bytes += 4; i++ }
    else bytes += 3
    if (bytes > limit) return bytes
  }
  return bytes
}

function byteCounter({ gen }: KeywordCxt): Name {
  return gen.scopeValue("func", { ref: utf8Bytes, code: new _Code(utf8Bytes.toString()) })
}

/** Source-schema refinements shared by every standalone protocol validator. */
export function addSchemaBudgets(ajv: Ajv2020): void {
  ajv.addKeyword({
    keyword: "x-rw-max-json-bytes", schemaType: "number",
    metaSchema: { type: "integer", minimum: 0 },
    code(ctx) {
      const count = ctx.gen.scopeValue("func", { ref: jsonEncodedBytes, code: new _Code(jsonEncodedBytes.toString()) })
      ctx.fail(_`${count}(${ctx.data}, ${ctx.schema}) > ${ctx.schema}`)
    },
  })
  ajv.addKeyword({
    keyword: "x-rw-max-utf8-bytes", type: "string", schemaType: "number",
    metaSchema: { type: "integer", minimum: 0 },
    code(ctx) {
      const bytes = byteCounter(ctx)
      ctx.fail(_`${bytes}(${ctx.data}, ${ctx.schema}) > ${ctx.schema}`)
    },
  })
  ajv.addKeyword({
    keyword: "x-rw-item-budget", type: "object", schemaType: "object",
    metaSchema: {
      type: "object", additionalProperties: false,
      required: ["array", "identity", "fields", "maxUtf8Bytes"],
      properties: {
        array: { type: "string", minLength: 1 }, identity: { type: "string", minLength: 1 },
        fields: { type: "array", minItems: 0, maxItems: 32, uniqueItems: true, items: { type: "string", minLength: 1 } },
        maxUtf8Bytes: { type: "integer", minimum: 0 },
      },
    },
    code(ctx) {
      const { gen, data, schema, parentSchema } = ctx
      const { array, identity, fields, maxUtf8Bytes } = schema as {
        array: string; identity: string; fields: string[]; maxUtf8Bytes: number
      }
      const maxItems: unknown = parentSchema.properties?.[array]?.maxItems
      if (typeof maxItems !== "number" || !Number.isSafeInteger(maxItems) || maxItems < 0) {
        throw new Error("x-rw-item-budget requires a direct finite array maxItems")
      }
      const items = gen.const("items", _`${data}[${array}]`)
      const valid = gen.let("budgetValid", _`Array.isArray(${items}) && ${items}.length <= ${maxItems}`)
      const bytes = byteCounter(ctx)
      gen.if(valid, () => {
        const seen = gen.const("identities", _`new Set()`)
        const total = gen.let("totalBytes", 0)
        gen.forOf("item", items, item => {
          gen.if(_`typeof ${item} !== "object" || ${item} === null || Array.isArray(${item})`, () => {
            gen.assign(valid, false); gen.break()
          })
          const id = gen.const("identity", _`${item}[${identity}]`)
          gen.if(_`typeof ${id} !== "string" || ${seen}.has(${id})`, () => {
            gen.assign(valid, false); gen.break()
          })
          gen.code(_`${seen}.add(${id})`)
          for (const field of fields) {
            const value = gen.const("value", _`${item}[${field}]`)
            gen.if(_`typeof ${value} !== "string"`, () => { gen.assign(valid, false); gen.break() })
            gen.add(total, _`${bytes}(${value}, ${maxUtf8Bytes} - ${total})`)
            gen.if(_`${total} > ${maxUtf8Bytes}`, () => { gen.assign(valid, false); gen.break() })
          }
        })
      })
      ctx.fail(_`!${valid}`)
    },
  })
}
