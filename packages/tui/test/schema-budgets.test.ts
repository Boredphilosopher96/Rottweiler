import { mkdtemp, writeFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { expect, test } from "bun:test"
import Ajv2020 from "ajv/dist/2020"
import { addSchemaBudgets } from "../scripts/schema-budgets"
import { standaloneValidator } from "../scripts/standalone-validator"

const schema = {
  type: "object", additionalProperties: false, required: ["entries"],
  properties: { entries: {
    type: "array", maxItems: 4, items: {
      type: "object", required: ["key", "text"], additionalProperties: false,
      properties: { key: { type: "string" }, text: { type: "string", "x-rw-max-utf8-bytes": 8 } },
    },
  } },
  "x-rw-item-budget": { array: "entries", identity: "key", fields: ["key", "text"], maxUtf8Bytes: 12 },
}

test("source-schema UTF8 and aggregate identity budgets survive standalone compilation", async () => {
  const ajv = new Ajv2020()
  addSchemaBudgets(ajv)
  const inProcess = ajv.compile(schema)
  const emitted = await standaloneValidator({ schema, typeName: "Fixture", typeImport: "fixture", banner: "" })
  const directory = await mkdtemp(join(tmpdir(), "rw-schema-budget-"))
  let standalone: (value: unknown) => boolean
  try {
    const path = join(directory, "validator.mjs")
    await writeFile(path, emitted.javascript)
    standalone = (await import(path)).default
  } finally { await rm(directory, { recursive: true, force: true }) }
  for (const validate of [inProcess, standalone]) {
    expect(validate({ entries: [{ key: "a", text: "😀😀" }] })).toBe(true)
    expect(validate({ entries: [{ key: "a", text: "😀😀x" }] })).toBe(false)
    expect(validate({ entries: [{ key: "a", text: "éééé" }, { key: "b", text: "éé" }] })).toBe(false)
    expect(validate({ entries: [{ key: "same", text: "" }, { key: "same", text: "" }] })).toBe(false)
    expect(validate({ entries: Array.from({ length: 5 }, (_, i) => ({ key: String(i), text: "" })) })).toBe(false)
    expect(validate({ entries: [{ key: "a", text: null }] })).toBe(false)
    expect(validate({ entries: [null] })).toBe(false)
    expect(validate({})).toBe(false)
    expect(validate({ entries: [{ key: "a", text: "\ud800\ud800" }] })).toBe(true)
    expect(validate({ entries: [{ key: "a", text: "\ud800\ud800\ud800" }] })).toBe(false)
  }
})

test("budget schemas cannot omit the finite traversal bound", () => {
  const ajv = new Ajv2020()
  addSchemaBudgets(ajv)
  const unbounded = structuredClone(schema)
  delete (unbounded.properties.entries as { maxItems?: number }).maxItems
  expect(() => ajv.compile(unbounded)).toThrow("direct finite array maxItems")
})


test("nested JSON byte budgets reuse the exact early-exit source visitor in standalone output", async () => {
  const schema = { type: "object", "x-rw-max-json-bytes": 32 }
  const emitted = await standaloneValidator({ schema, typeName: "Fixture", typeImport: "fixture", banner: "" })
  const directory = await mkdtemp(join(tmpdir(), "rw-schema-json-budget-"))
  try {
    const path = join(directory, "validator.mjs")
    await writeFile(path, emitted.javascript)
    const validate = (await import(path)).default as (value: unknown) => boolean
    for (const text of ["", "a".repeat(24), "a".repeat(25), "\\\"\n", "é雪😀", "\ud800".repeat(5)]) {
      const value = { a: text }
      expect(validate(value)).toBe(Buffer.byteLength(JSON.stringify(value)) <= 32)
    }
    expect(validate({ a: "x".repeat(1_000_000) })).toBe(false)
    expect(validate({ a: [1, { b: [true, false, null, "large"] }] })).toBe(false)
  } finally { await rm(directory, { recursive: true, force: true }) }
})

test("identity-only budgets require no artificial text fields", () => {
  const ajv = new Ajv2020()
  addSchemaBudgets(ajv)
  const identitySchema = structuredClone(schema)
  identitySchema["x-rw-item-budget"].fields = []
  identitySchema["x-rw-item-budget"].maxUtf8Bytes = 0
  const validate = ajv.compile(identitySchema)
  expect(validate({ entries: [{ key: "one", text: "" }, { key: "two", text: "" }] })).toBe(true)
  expect(validate({ entries: [{ key: "one", text: "" }, { key: "one", text: "" }] })).toBe(false)
})
