import { familyControlsReader } from "../family-controls-reader"
import { EngineHttpSseClient } from "../transport"
import { sessionReader } from "../session-reader-factory"
import { CLIENT_COMMAND_EXECUTION, PROTOCOL_VERSION, TRANSCRIPT_PROJECTION_VERSION, type ClientCommand, type CommandReply, type EngineEvent, type TranscriptPage } from "../protocol"
import type { ClientAllocationOwner } from "../client-allocation"
import type { ReplyAllocation } from "../transport/reply-allocation"

export const MEMORY_LOAD = { historyRows: 10_000, pageRows: 32, previewBytes: 4096, bodyBytes: 256 * 1024, catalogRows: 128, draftBytes: 64 * 1024, toolInvocations: 16, toolChunks: 4, toolChunkBytes: 16 * 1024, questions: 4 } as const
export const MEMORY_CHILD = { session_id: "child-0", ancestry: [{ subagent_id: "agent-0", session_id: "child-0" }] }
const SESSION = "memory-probe"
const SOURCE = "20000"
const emitted_at = "2026-09-06T00:00:00Z"

/** Fixed-size protocol fixture. No history vector is retained in the server. */
export class MemoryFixture {
  readonly client: EngineHttpSseClient
  readonly family: ReturnType<typeof familyControlsReader>
  #watch: (() => void) | null = null
  readonly reader: ReturnType<typeof sessionReader>
  readonly #server: ReturnType<typeof Bun.serve>
  #request = 0
  #cycle = 0
  #held = false
  #pending = new Set<() => void>()
  #invalid = false
  requests = 0
  resolvedChildControls = 0
  get pending(): number { return this.#pending.size }
  set cycle(value: number) { this.#cycle = value }
  constructor(socketPath: string, allocations: ClientAllocationOwner) {
    this.#server = Bun.serve({ unix: socketPath, fetch: request => this.#reply(request) })
    this.client = new EngineHttpSseClient({ socketPath, bootstrapToken: "memory-fixture", allocations })
    const read = async (command: ClientCommand, signal: AbortSignal, allocation: ReplyAllocation): Promise<Extract<CommandReply, { type: "read" }>> => {
      const reply = await this.client.postCommand(command, signal, allocation)
      if (reply.type !== "read" || reply.outcome.type !== "accepted") throw new Error("memory fixture read failed")
      return reply
    }
    this.reader = sessionReader(read, () => this.meta())
    this.family = familyControlsReader(read, () => this.meta())
  }
  connectionCompletion(command: ClientCommand): EngineEvent | null {
    return command.type === "get_session_review" ? this.#event(command) : null
  }
  meta(): ClientCommand["meta"] { return { protocol_version: PROTOCOL_VERSION, client_id: "memory-client", request_id: `memory-${++this.#request}` } }
  historyReady(): Extract<EngineEvent, { type: "session_history_ready" }> {
    return { type: "session_history_ready", meta: { ...this.meta(), emitted_at }, session_id: SESSION, through_sequence: SOURCE }
  }
  hold(): void { this.#held = true }
  invalidateNext(): void { this.#invalid = true }
  release(): void { this.#held = false; for (const resolve of this.#pending) resolve(); this.#pending.clear() }
  async close(): Promise<void> { this.#watch?.(); this.#watch = null; this.release(); await this.#server.stop(true) }
  async command(command: ClientCommand, allocation: ReplyAllocation): Promise<CommandReply> { return this.client.postCommand(command, undefined, allocation) }
  async #reply(request: Request): Promise<Response> {
    if (new URL(request.url).pathname === "/v1/connect") return Response.json({ client_id: "memory-client", token: "memory-token" })
    if (request.headers.get("authorization") !== "Bearer memory-token") return new Response("unauthorized", { status: 401 })
    const command = await request.json() as ClientCommand
    this.requests++
    if (command.type === "resolve_child_control") this.resolvedChildControls++
    if (command.type === "read_family_controls" && command.after_revision !== null) await this.#waitWatch(request.signal)
    if (this.#held && command.type !== "read_family_controls") await new Promise<void>(resolve => { this.#pending.add(resolve) })
    if (this.#invalid) { this.#invalid = false; return Response.json({ type: "read", outcome: { type: "accepted" }, events: [{ type: "unsupported_probe_event" }] }) }
    const reply: CommandReply = CLIENT_COMMAND_EXECUTION[command.type] === "read"
      ? { type: "read", outcome: { type: "accepted" }, events: [this.#event(command)] }
      : { type: "command", outcome: command.type === "send_message"
        ? { type: "rejected", error: { category: "protocol", code: "probe_rejected", message: "declined ".repeat(8192), retryable: false } }
        : { type: "accepted" } }
    return Response.json(reply)
  }
  #waitWatch(signal: AbortSignal): Promise<void> {
    this.#watch?.()
    return new Promise(resolve => {
      const finish = () => { clearTimeout(timer); signal.removeEventListener("abort", finish); if (this.#watch === finish) this.#watch = null; resolve() }
      const timer = setTimeout(finish, 10_000)
      this.#watch = finish
      if (signal.aborted) finish(); else signal.addEventListener("abort", finish, { once: true })
    })
  }
  #event(command: ClientCommand): EngineEvent {
    const meta = { ...command.meta, emitted_at }
    switch (command.type) {
      case "read_transcript": return { type: "transcript_page_ready", meta, session_id: command.session_id, result: { type: "ready", page: this.#page(command.session_id, command.read) } }
      case "read_transcript_content": {
        const offset = command.read.offset
        const count = Math.min(command.read.max_bytes, MEMORY_LOAD.bodyBytes - offset)
        return { type: "transcript_content_ready", meta, session_id: SESSION, page: { view: command.read.view, source: command.read.source,
          offset, next_offset: offset + count < MEMORY_LOAD.bodyBytes ? offset + count : null, total_bytes: MEMORY_LOAD.bodyBytes,
          text: "canonical output ".repeat(Math.ceil(count / 17)).slice(0, count), format: "text" } }
      }
      case "read_transcript_tail": {
        const part = command.read.part
        return { type: "transcript_tail_ready", meta, session_id: command.session_id, result: { type: "ready", page: {
          view: this.#page(command.session_id, { known_view: null, position: { type: "latest" }, max_items: 0, max_bytes: 1024 }).view,
          identity: { generation: "0", turn_started: null, response_epoch: null, tools_epoch: null },
          content: part.type === "text" || part.type === "thinking" ? { type: part.type, preview: { text: "", truncated: false } }
            : { type: part.type, offset: part.offset, items: [], next_offset: null },
        } } }
      }
      case "get_session_controls": return { type: "session_controls_ready", meta, session_id: SESSION, snapshot: { through: String(20000 + MEMORY_LOAD.toolInvocations * (1 + MEMORY_LOAD.toolChunks)),
        controls: { approvals: [], pending_plan: null, questions: Array.from({ length: MEMORY_LOAD.questions }, (_, index) => ({
          question_id: `question-${index}`, turn_id: "probe-turn", question: { id: `question-${index}`, prompt: `Decision ${index} ${"context ".repeat(512)}`,
            response_kind: "select_one", options: [{ value: "yes", label: "Continue", description: "Proceed with this decision" }] },
        })) } } }
      case "get_todos": return { type: "todos_read", meta, session_id: command.session_id, result: { type: "ready", todos: { through: SOURCE, snapshot: { items: [] } } } }
      case "read_session_children": return { type: "session_children_ready", meta, session_id: command.session_id, result: { type: "ready", snapshot: { through: SOURCE, children: [] } } }
      case "list_runtime_services": return { type: "runtime_services_listed", meta, session_id: SESSION, services: [] }
      case "list_models": return { type: "models_listed", meta, models: [], aliases: [], providers: [], cached: false, truncated: false }
      case "get_session_review": return { type: "session_review_ready", meta, session_id: SESSION,
        review: { session_id: SESSION, files: [{ path: "held-review.txt", unified_diff: "--- a/held-review.txt\n+++ b/held-review.txt\n@@ -1,256 +1,256 @@\n" + "-old content\n+new content\n".repeat(256),
          status: "pending", truncated: false, unrestorable_reason: null, original_hash: "old", current_hash: "new" }] } }
      case "get_ui_catalog": return { type: "ui_catalog_ready", meta, session_id: SESSION, catalog: { entries: [] } }
      case "get_ui_panels": return { type: "ui_panels_ready", meta, session_id: SESSION, panels: { panels: [] } }
      case "read_family_controls": return { type: "family_controls_ready", meta, session_id: SESSION, snapshot: { revision: "1", children: [{ target: MEMORY_CHILD,
        controls: { revision: "1", through: SOURCE, questions: 1, approvals: 0, pending_plan: false, available: true },
      }] } }
      case "read_child_controls": return { type: "child_controls_ready", meta, session_id: SESSION, target: command.target,
        snapshot: { revision: "1", snapshot: { through: SOURCE, controls: { approvals: [], pending_plan: null, questions: [{ question_id: "child-question", turn_id: "child-turn",
          question: { id: "child-question", prompt: "Which child file?", response_kind: "text", options: [] },
        }] } } } }
      case "resolve_child_read_scope": return { type: "child_read_scope_ready", meta, session_id: SESSION, target: command.target,
        result: { type: "ready", scope: { type: "descendant", root_session_id: SESSION, ancestry: [{ subagent_id: "agent-0", session_id: "child-0", source_sequence: "1" }] } } }
      case "read_child_state": return { type: "child_state_ready", meta, session_id: SESSION, target: command.target,
        snapshot: { through: SOURCE, driver_client_id: "memory-client", title: "Memory child", model_alias: "fast", provider: null, thinking: "off", mode_id: "execute",
          active_turn: null, completed_turns: "0", shell: null, compaction: null, plugin_statuses: [], queued_messages: [], budget: null } }
      case "list_subagents": return { type: "subagents_listed", meta, session_id: SESSION, subagents: Array.from({ length: MEMORY_LOAD.catalogRows }, (_, index) => ({
        subagent_id: `agent-${index}`, child_session_id: `child-${index}`, task: `${this.#cycle}:${index} ${"task ".repeat(200)}`, agent: "reviewer", model: "fast", isolation: "shared", activity: "idle",
      })) }
      default: throw new Error(`unconfigured memory fixture query ${command.type}`)
    }
  }
  #page(sessionId: string, read: Extract<ClientCommand, { type: "read_transcript" }>["read"]): TranscriptPage {
    const count = Math.min(read.max_items, MEMORY_LOAD.pageRows)
    const position = read.position
    const first = position.type === "latest" ? MEMORY_LOAD.historyRows - count
      : position.type === "at_ordinal" ? Math.min(Number(position.ordinal), MEMORY_LOAD.historyRows - count) : 0
    return {
      view: { session_id: sessionId, generation: "0", through: SOURCE, projection_version: TRANSCRIPT_PROJECTION_VERSION, digest: Array(32).fill(0) as TranscriptPage["view"]["digest"] },
      first_ordinal: String(first), total_items: String(MEMORY_LOAD.historyRows), anchor: { type: "unspecified" }, invalidation: { type: "none" },
      items: Array.from({ length: count }, (_, index) => {
        const id = String(first + index)
        return { id, ordinal: id, revision: id, agent_turn: null, content: { type: "command", name: `probe-${id}`, message: {
          text: `${this.#cycle}:${id} ${"bounded history ".repeat(255)}`, format: "text", complete: true, source: { sequence: id, selector: { type: "command_message" } },
        } } }
      }),
    }
  }
}
