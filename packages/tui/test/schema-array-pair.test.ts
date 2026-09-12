import { expect, test } from "bun:test"
import Ajv2020 from "ajv/dist/2020"
import { mkdtemp, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { addSchemaBudgets } from "../scripts/schema-budgets"
import { standaloneValidator } from "../scripts/standalone-validator"

const rule = {
  left: ["declaration", "fields"], right: ["result", "fields"],
  identity: "key", discriminator: "type", maxItems: 4,
  collections: [
    { tag: "sequence", values: "values", limit: "capacity" },
    { tag: "matrix", values: "rows", limit: "capacity", width: "columns" },
  ],
}
const schema = { type: "object", "x-rw-array-pair": rule }
const pair = (left: unknown[], right: unknown[]) => ({
  declaration: { fields: left }, result: { fields: right },
})

test("bounded positional correspondence is identical in emitted validators", async () => {
  const ajv = new Ajv2020()
  addSchemaBudgets(ajv)
  const emitted = await standaloneValidator({ schema, typeName: "Fixture", typeImport: "fixture", banner: "" })
  const directory = await mkdtemp(join(tmpdir(), "rw-array-pair-"))
  try {
    const path = join(directory, "validator.mjs")
    await writeFile(path, emitted.javascript)
    const standalone = (await import(path)).default as (value: unknown) => boolean
    const text = { key: "a", type: "scalar" }, second = { key: "b", type: "scalar" }
    const list = { key: "a", type: "sequence", capacity: 1 }
    const table = { key: "a", type: "matrix", capacity: 2, columns: ["one", "two"] }
    const cases: [unknown, boolean][] = [
      [pair([], []), true], [pair([text], [text]), true],
      [pair([text, second], [text, second]), true],
      [pair([text], []), false], [pair([], [text]), false],
      [pair([text, second], [second, text]), false],
      [pair([text, text], [text, text]), false],
      [pair([text], [second]), false],
      [pair([text], [{ ...text, type: "other" }]), false],
      [pair([text], [null]), false], [pair([null], [text]), false],
      [pair(Array(5).fill(text), Array(5).fill(text)), false],
      [pair([list], [{ key: "a", type: "sequence", values: ["x"] }]), true],
      [pair([list], [{ key: "a", type: "sequence", values: ["x", "y"] }]), false],
      [pair([table], [{ key: "a", type: "matrix", rows: [["x", "y"], ["x", "y"]] }]), true],
      [pair([table], [{ key: "a", type: "matrix", rows: [["x"]] }]), false],
      [pair([table], [{ key: "a", type: "matrix", rows: [null] }]), false],
      [pair([table], [{ key: "a", type: "matrix", rows: Array(3).fill(["x", "y"]) }]), false],
      [{}, false], [{ declaration: null, result: {} }, false],
    ]
    for (const validate of [ajv.compile(schema), standalone]) {
      for (const [input, expected] of cases) expect(validate(input)).toBe(expected)
    }
  } finally { await rm(directory, { recursive: true, force: true }) }
})

test("correspondence schemas require explicit bounded traversal", () => {
  const ajv = new Ajv2020()
  addSchemaBudgets(ajv)
  for (const invalid of [
    { ...rule, maxItems: 129 }, { ...rule, maxItems: -1 },
    { ...rule, left: Array(9).fill("field") }, { ...rule, left: [] },
    { ...rule, collections: Array(9).fill(rule.collections[0]) },
  ]) expect(() => ajv.compile({ ...schema, "x-rw-array-pair": invalid })).toThrow()
})
