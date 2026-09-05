import { expect, test } from "bun:test"
import { mkdtemp, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { standaloneValidator } from "../scripts/standalone-validator"

test("standalone generation discovers nested discriminator names and preserves declared open values", async () => {
  const schema = {
    oneOf: [{
      type: "object", additionalProperties: false, required: ["hook", "result"],
      properties: {
        hook: { const: "before" },
        result: { oneOf: [
          { type: "object", additionalProperties: false, required: ["decision"], properties: { decision: { const: "continue" } } },
          { type: "object", additionalProperties: false, required: ["decision", "payload"], properties: { decision: { const: "replace" }, payload: {} } },
        ] },
      },
    }, {
      type: "object", additionalProperties: false, required: ["hook", "kind"],
      properties: { hook: { const: "after" }, kind: { type: "string", maxLength: 2 } },
    }],
  }
  const first = await standaloneValidator({ schema, typeName: "Hook", typeImport: "./hook", banner: "// generated\n" })
  const second = await standaloneValidator({ schema, typeName: "Hook", typeImport: "./hook", banner: "// generated\n" })
  expect(first).toEqual(second)
  expect(first.declaration).toContain('import type { Hook } from "./hook"')
  const directory = await mkdtemp(join(tmpdir(), "rw-validator-"))
  try {
    const path = join(directory, "validator.mjs")
    await writeFile(path, first.javascript)
    const module = await import(path)
    const validate: (value: unknown) => boolean = module.default
    expect(validate({ hook: "before", result: { decision: "continue" } })).toBe(true)
    expect(validate({ hook: "before", result: { decision: "replace", payload: { arbitrary: [1, "yes"] } } })).toBe(true)
    expect(validate({ hook: "after", kind: "界" })).toBe(true)
    for (const value of [
      { hook: "unknown" },
      { hook: "before", result: { decision: "continue", payload: {} } },
      { hook: "before", result: { decision: "replace" } },
      { hook: "after", kind: "abc" },
    ]) expect(validate(value)).toBe(false)
  } finally { await rm(directory, { recursive: true, force: true }) }
})
