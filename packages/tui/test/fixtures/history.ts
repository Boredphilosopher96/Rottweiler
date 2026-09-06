import { TRANSCRIPT_PROJECTION_VERSION, type TranscriptPage, type TranscriptRead } from "../../src/protocol"

export function fixturePage(session: string, read: TranscriptRead): TranscriptPage {
  const position = read.position
  const first = position.type === "latest" ? 968 : position.type === "at_ordinal" ? Math.min(968, Number(position.ordinal))
    : position.type === "around" ? Math.max(0, Math.min(968, Number(position.item) - 16)) : 0
  return {
    view: { session_id: session, projection_version: TRANSCRIPT_PROJECTION_VERSION, generation: "0", through: "2000", digest: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] },
    first_ordinal: String(first), total_items: "1000", anchor: { type: "unspecified" }, invalidation: { type: "none" },
    items: Array.from({ length: 32 }, (_, index) => {
      const id = String(first + index)
      return {
        id, ordinal: id, revision: id, agent_turn: null,
        content: {
          type: "command", name: `row-${id}`, message: {
            text: `Visible history body ${id}`, format: "text", complete: true,
            source: { sequence: id, selector: { type: "command_message" } }
          }
        }
      }
    }),
  }
}

/** Explicit read capability for interaction tests that have no historical content. */
export const emptySessionReader: import("../../src/session-reader").SessionReader = {
  children: async () => ({ type: "ready", snapshot: { through: null, children: [] } }),
  tail: async () => { throw new Error("this fixture has no live tail") },
  uiCatalog: async () => ({ entries: [] }),
  uiPanels: async () => ({ panels: [] }),
  todos: async () => ({ type: "ready", todos: { through: "1000", snapshot: { items: [] } } }),
  page: async ({ sessionId: session }, read) => {
    const page = fixturePage(session, read)
    return { type: "ready", page: { ...page, first_ordinal: "0", total_items: "0", items: [] } }
  },
  content: async () => { throw new Error("this fixture has no historical content") },
}

export function conversationItem(id: number, role: "user" | "assistant", text: string, reasoning = ""): import("../../src/protocol").TranscriptItem {
  const sequence = String(id)
  return {
    id: sequence, ordinal: "0", revision: sequence, agent_turn: sequence,
    content: {
      type: "conversation", role, omitted_blocks: false,
      source: { sequence, selector: { type: "conversation" } },
      blocks: [
        ...(reasoning.length === 0 ? [] : [{
          type: "reasoning" as const, body: {
            text: reasoning, format: "text" as const, complete: true,
            source: { sequence, selector: { type: "conversation_block" as const, index: 1 } },
          }
        }]),
        {
          type: "text", body: {
            text, format: "text", complete: true,
            source: { sequence, selector: { type: "conversation_block", index: 0 } }
          }
        },
      ],
    },
  }
}

/** Native semantic fixture data, kept outside the app's bounded read cache. */
export function sessionReaderFor(items: readonly import("../../src/protocol").TranscriptItem[], head?: () => Pick<import("../../src/protocol").TranscriptView, "generation" | "through">): import("../../src/session-reader").SessionReader {
  return {
    children: emptySessionReader.children,
    tail: emptySessionReader.tail,
    todos: emptySessionReader.todos,
    uiCatalog: emptySessionReader.uiCatalog,
    uiPanels: emptySessionReader.uiPanels,
    page: async ({ sessionId: session }, read) => {
      const position = read.position
      const limit = Math.min(read.max_items, items.length)
      const anchor = position.type === "around" || position.type === "before" || position.type === "after"
        ? items.findIndex(item => item.id === position.item) : -1
      const requested = position.type === "latest" ? items.length - limit
        : position.type === "at_ordinal" ? Number(position.ordinal)
          : position.type === "around" ? anchor - Math.floor(limit / 2)
            : position.type === "before" ? anchor - limit
              : position.type === "after" ? anchor + 1 : 0
      const first = Math.max(0, Math.min(items.length - limit, requested))
      const selected = items.slice(first, first + limit).map((item, index) => ({ ...item, ordinal: String(first + index) }))
      const base = fixturePage(session, read)
      const anchored = items[anchor]
      return {
        type: "ready", page: {
          ...base, first_ordinal: String(first), total_items: String(items.length), items: selected,
          view: { ...base.view, through: String(items.reduce((largest, item) => BigInt(item.revision) > largest ? BigInt(item.revision) : largest, 0n)), ...head?.() },
          anchor: anchored === undefined ? { type: "unspecified" } : { type: "exact", item: anchored.id },
        }
      }
    },
    content: async () => { throw new Error("this fixture does not provide complete content") },
  }
}

export function toolItem(id: number, name: string, argumentsText: string, output?: string): import("../../src/protocol").TranscriptItem {
  const sequence = String(id)
  return {
    id: sequence, ordinal: "0", revision: sequence, agent_turn: "1", content: {
      type: "tool", invocation_id: `invocation-${id}`, name, call_index: 0,
      arguments: {
        text: argumentsText, format: "json", complete: true,
        source: { sequence, selector: { type: "tool_arguments" } }
      }, diff: null,
      status: output === undefined ? { type: "running" } : {
        type: "finished", presentation: null, is_error: false,
        output: { text: output, format: "text", complete: true, source: { sequence, selector: { type: "tool_output" } } }
      },
    }
  }
}

export function commandItem(id: number, name: string, text: string): import("../../src/protocol").TranscriptItem {
  const sequence = String(id)
  return {
    id: sequence, ordinal: "0", revision: sequence, agent_turn: null, content: {
      type: "command", name, message: {
        text, format: "text", complete: true,
        source: { sequence, selector: { type: "command_message" } }
      },
    }
  }
}

export function shellItem(id: number, command: string, output?: string): import("../../src/protocol").TranscriptItem {
  const sequence = String(id)
  return {
    id: sequence, ordinal: "0", revision: output === undefined ? sequence : String(id + 1), agent_turn: null, content: {
      type: "shell", active: output === undefined, status: output === undefined ? null : 0,
      command: { text: command, format: "text", complete: true, source: { sequence, selector: { type: "shell_command" } } },
      output: output === undefined ? null : { text: output, format: "text", complete: true, source: { sequence: String(id + 1), selector: { type: "shell_output" } } },
    }
  }
}

/** History refresh includes a real coalescing timer; wait for completion, not a frame count. */
export async function waitForHistory(
  setup: import("@opentui/core/testing").TestRendererSetup,
  predicate: () => boolean,
): Promise<void> {
  const deadline = performance.now() + 1000
  while (!predicate()) {
    if (performance.now() > deadline) throw new Error("history did not reach its expected source revision")
    await Bun.sleep(1)
    await setup.renderOnce()
    await setup.flush()
  }
  await setup.renderOnce()
  await setup.flush()
}
