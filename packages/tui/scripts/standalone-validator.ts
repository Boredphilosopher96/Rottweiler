import { readFile } from "node:fs/promises"
import { fileURLToPath } from "node:url"
import Ajv2020 from "ajv/dist/2020"
import standaloneCode from "ajv/dist/standalone"

function schemaObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

/** Discover a unique, required string tag shared by every alternative. */
function addDiscriminators(value: unknown): void {
  if (Array.isArray(value)) {
    for (const child of value) addDiscriminators(child)
    return
  }
  if (!schemaObject(value)) return
  for (const child of Object.values(value)) addDiscriminators(child)
  const variants = value.oneOf
  if (!Array.isArray(variants) || variants.length === 0) return
  const first = variants[0]
  if (!schemaObject(first) || !schemaObject(first.properties)) return
  for (const propertyName of Object.keys(first.properties)) {
    const tags = new Set<string>()
    for (const variant of variants) {
      if (!schemaObject(variant) || !schemaObject(variant.properties)
        || !Array.isArray(variant.required) || !variant.required.includes(propertyName)) break
      const tag = variant.properties[propertyName]
      if (!schemaObject(tag) || typeof tag.const !== "string" || tags.has(tag.const)) break
      tags.add(tag.const)
    }
    if (tags.size !== variants.length) continue
    value.type = "object"
    value.discriminator = { propertyName }
    return
  }
}

export interface StandaloneValidatorOptions {
  readonly schema: object
  readonly typeName: string
  readonly typeImport: string
  readonly banner: string
}

/** Compile-time AJV ownership; emitted validators contain no runtime package dependency. */
export async function standaloneValidator({ schema, typeName, typeImport, banner }: StandaloneValidatorOptions): Promise<{
  readonly javascript: string
  readonly declaration: string
}> {
  const bunVersion = (await readFile(new URL("../../../.bun-version", import.meta.url), "utf8")).trim()
  if (Bun.version !== bunVersion) throw new Error(`validator generation requires Bun ${bunVersion}`)
  const ajv = new Ajv2020({
    code: { source: true, esm: true, lines: true },
    discriminator: true,
    inlineRefs: false,
    allErrors: false,
    messages: false,
    // Schemars' numeric format names are annotations; its type/min/max constraints
    // remain enforced. Runtime protocol version and u64 cursors use shared owners.
    validateFormats: false,
  })
  const prepared = structuredClone(schema)
  addDiscriminators(prepared)
  const validate = ajv.compile(prepared)
  const generated = standaloneCode(ajv, validate)
  const bundled = await Bun.build({
    entrypoints: ["rottweiler:standalone-validator"],
    root: fileURLToPath(new URL("../", import.meta.url)),
    target: "browser",
    format: "esm",
    minify: { whitespace: true, syntax: true, identifiers: false },
    plugins: [{ name: "generated-validator", setup(build) {
      build.onResolve({ filter: /^rottweiler:/ }, () => ({ path: "validator.js", namespace: "generated" }))
      build.onLoad({ filter: /.*/, namespace: "generated" }, () => ({ contents: generated, loader: "js" }))
      build.onResolve({ filter: /^ajv\// }, args => ({ path: fileURLToPath(import.meta.resolve(args.path)) }))
    } }],
  })
  if (!bundled.success || bundled.outputs.length !== 1) throw new Error("standalone validator bundling failed")
  const javascript = await bundled.outputs[0]?.text()
  if (javascript === undefined) throw new Error("standalone validator output missing")
  return {
    javascript: banner + javascript,
    declaration: `${banner}import type { ${typeName} } from ${JSON.stringify(typeImport)};\nexport default function validate(value: unknown): value is ${typeName};\n`,
  }
}
