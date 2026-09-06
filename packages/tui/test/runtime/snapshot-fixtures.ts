import { TRANSCRIPT_PROJECTION_VERSION, type ClientCommand, type CommandReply, type TranscriptView } from "../../src/protocol"

export function tailReply(command: Extract<ClientCommand, { type: "read_transcript_tail" }>): CommandReply {
  const part = command.read.part
  return { type: "read", outcome: { type: "accepted" }, events: [{ type: "transcript_tail_ready",
    meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: command.session_id,
    result: { type: "ready", page: {
      identity: { generation: "0", turn_started: null, response_epoch: "0", tools_epoch: "0" },
      view: { session_id: command.session_id, projection_version: TRANSCRIPT_PROJECTION_VERSION, generation: "0", through: null, digest: Array(32).fill(0) as TranscriptView["digest"] },
      content: part.type === "text" || part.type === "thinking" ? { type: part.type, preview: { text: "", truncated: false } }
        : { type: part.type, offset: part.offset, items: [], next_offset: null },
    } },
  }] }
}

export function todosReply(command: Extract<ClientCommand, { type: "get_todos" }>): CommandReply {
  return { type: "read", outcome: { type: "accepted" }, events: [{ type: "todos_read",
    meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: command.session_id,
    result: { type: "ready", todos: { through: "5", snapshot: { items: [] } } },
  }] }
}

/** Empty committed source before a mock stream delivers its scripted events. */
export function emptyBootstrapReply(command: ClientCommand): CommandReply | undefined {
  const common = { meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" } }
  if (command.type === "read_transcript_tail") return tailReply(command)
  if (command.type === "get_todos") return { type: "read", outcome: { type: "accepted" }, events: [{ ...common,
    type: "todos_read", session_id: command.session_id, result: { type: "ready", todos: { through: null, snapshot: { items: [] } } },
  }] }
  if (command.type === "get_session_controls") return { type: "read", outcome: { type: "accepted" }, events: [{ ...common,
    type: "session_controls_ready", session_id: command.session_id, snapshot: { through: null, controls: { questions: [], approvals: [], pending_plan: null } },
  }] }
  if (command.type === "read_session_children") return { type: "read", outcome: { type: "accepted" }, events: [{ ...common,
    type: "session_children_ready", session_id: command.session_id, result: { type: "ready", snapshot: { through: null, children: [] } },
  }] }
  if (command.type === "get_session_state") return { type: "read", outcome: { type: "accepted" }, events: [{ ...common,
    type: "session_state_ready", session_id: command.session_id, snapshot: {
      through: null, driver_client_id: command.meta.client_id, title: null, model_alias: "main", provider: null, thinking: "off", mode_id: "execute",
      active_turn: null, completed_turns: "0", shell: null, compaction: null, plugin_statuses: [], queued_messages: [], budget: null,
    },
  }] }
  return undefined
}
