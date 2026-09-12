import type { ClientCommand, CommandReply } from "./protocol"
import type { SessionReader } from "./session-reader"
import type { ReplyAllocation } from "./transport/reply-allocation"

type SessionReadCommand = Extract<ClientCommand, { type: "read_session_children" | "read_transcript_tail" | "read_transcript" | "read_transcript_content" | "get_todos" | "get_ui_catalog" | "get_ui_panels" }>

/** Typed result correlation shared by every session read capability. */
export function sessionReader(
  readSession: (command: SessionReadCommand, signal: AbortSignal, allocation: ReplyAllocation) => Promise<Extract<CommandReply, { type: "read" }>>,
  meta: () => ClientCommand["meta"],
): SessionReader {
  return {
    children: async ({ sessionId, scope }, signal, allocation) => {
      const reply = await readSession({ type: "read_session_children", meta: meta(), session_id: sessionId, scope }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "session_children_ready" || event.session_id !== sessionId) {
        throw new Error("children reply is missing its session-bound result")
      }
      return event.result
    },
    tail: async ({ sessionId, scope }, read, signal, allocation) => {
      const reply = await readSession({ type: "read_transcript_tail", meta: meta(), session_id: sessionId, scope, read }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "transcript_tail_ready" || event.session_id !== sessionId) {
        throw new Error("live tail reply is missing its session-bound result")
      }
      return event.result
    },
    uiCatalog: async (sessionId, signal, allocation) => {
      const reply = await readSession({ type: "get_ui_catalog", meta: meta(), session_id: sessionId }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "ui_catalog_ready" || event.session_id !== sessionId) {
        throw new Error("UI catalog reply is missing its session-bound result")
      }
      return event.catalog
    },
    uiPanels: async (sessionId, signal, allocation) => {
      const reply = await readSession({ type: "get_ui_panels", meta: meta(), session_id: sessionId }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "ui_panels_ready" || event.session_id !== sessionId) {
        throw new Error("UI panels reply is missing its session-bound result")
      }
      return event.panels
    },
    todos: async ({ sessionId, scope }, signal, allocation) => {
      const reply = await readSession({ type: "get_todos", meta: meta(), session_id: sessionId, scope }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "todos_read" || event.session_id !== sessionId) {
        throw new Error("task reply is missing its session-bound result")
      }
      return event.result
    },
    page: async ({ sessionId, scope }, read, signal, allocation) => {
      const reply = await readSession({ type: "read_transcript", meta: meta(), session_id: sessionId, scope, read }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "transcript_page_ready" || event.session_id !== sessionId) {
        throw new Error("transcript page reply is missing its result")
      }
      return event.result
    },
    content: async ({ sessionId, scope }, read, signal, allocation) => {
      const reply = await readSession({ type: "read_transcript_content", meta: meta(), session_id: sessionId, scope, read }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "transcript_content_ready" || event.session_id !== sessionId) {
        throw new Error("transcript content reply is missing its result")
      }
      return event.page
    },
  }
}
