import type { OutputSchema, ProviderRequest } from "./generated/provider-contract"

/** Finite schema input validation before invoking any plugin provider handler. */
export function validOutputContract(request: ProviderRequest): boolean {
  const output = request.output
  if (output.mode === "text") return true
  if (request.tools.length !== 0 || request.tool_choice.mode !== "none"
    || !/^[A-Za-z0-9_-]{1,64}$/.test(output.name) || output.schema.type !== "object") return false
  let nodes = 0
  const bytes = new TextEncoder()
  const walk = (schema: OutputSchema, depth: number): boolean => {
    if (++nodes > 256 || depth > 16) return false
    switch (schema.type) {
      case "array": return walk(schema.items, depth + 1)
      case "nullable": return walk(schema.value, depth + 1)
      case "object": {
        if (schema.fields.length > 64) return false
        const names = new Set<string>()
        for (const field of schema.fields) {
          if (field.name.length === 0 || field.name.length > 128 || bytes.encode(field.name).length > 128
            || names.has(field.name) || !walk(field.schema, depth + 1)) return false
          names.add(field.name)
        }
        return true
      }
      case "string": case "number": case "integer": case "boolean": case "null": return true
    }
  }
  return walk(output.schema, 1)
}
