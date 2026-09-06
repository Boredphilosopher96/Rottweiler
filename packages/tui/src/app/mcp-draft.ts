import type { ClientCommand } from "../protocol"

type EnvironmentEntries = Extract<ClientCommand, { type: "add_mcp_stdio_server" }>["environment"]

export const MCP_ENVIRONMENT_DRAFT_LIMITS = { entries: 128, bytes: 64 * 1024 } as const

export class McpEnvironmentDraft {
  readonly #values = new Map<string, string>()
  #bytes = 0

  set(key: string, value: string): boolean {
    const previous = this.#values.get(key)
    const added = Buffer.byteLength(key) + Buffer.byteLength(value)
    const removed = previous === undefined ? 0 : Buffer.byteLength(key) + Buffer.byteLength(previous)
    const nextBytes = this.#bytes - removed + added
    if (nextBytes > MCP_ENVIRONMENT_DRAFT_LIMITS.bytes ||
      (previous === undefined && this.#values.size === MCP_ENVIRONMENT_DRAFT_LIMITS.entries)) return false
    this.#values.set(key, value)
    this.#bytes = nextBytes
    return true
  }

  take(): EnvironmentEntries {
    const entries = Array.from(this.#values, ([key, value]) => ({ key, value }))
    this.clear()
    return entries
  }

  clear(): void {
    this.#values.clear()
    this.#bytes = 0
  }
}
