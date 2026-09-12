import type { HandlerContext } from "./server"
import type { ExtensionEventNotice, ExtensionEventChunk, PluginPushMethod, JsonValue } from "./generated/protocol-3"
import { RPC_METHODS } from "./generated/protocol-3"
import validateChunk from "./generated/extension-event-chunk-validator.js"

export interface EventHandlerContext extends HandlerContext {
  /** Reads only this active delivery's redacted JSON source, at most 64KiB per request. */
  readSource(offset: number, maxBytes: number): Promise<ExtensionEventChunk>
}

/** The host revokes the source when the callback settles. No arbitrary journal cursor is accepted. */
export function eventSourceReader(notice: ExtensionEventNotice, request: (method: PluginPushMethod, params: JsonValue) => Promise<JsonValue>): EventHandlerContext["readSource"] {
  return async (offset, maxBytes) => {
    if (!Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > 65_536) throw new Error("invalid event source read")
    const result = await request(RPC_METHODS.eventRead, { cursor: notice.cursor, offset, max_bytes: maxBytes })
    if (!validateChunk(result) || result.cursor.session_id !== notice.cursor.session_id || result.cursor.sequence !== notice.cursor.sequence || result.offset !== offset) throw new Error("invalid event source response")
    if (result.data_base64.length > Math.ceil(maxBytes / 3) * 4) throw new Error("event source encoded chunk exceeds limit")
    const bytes = Buffer.from(result.data_base64, "base64")
    if (bytes.length === 0 || bytes.length > maxBytes || bytes.toString("base64") !== result.data_base64 || (result.next_offset !== null && result.next_offset !== offset + bytes.length)) throw new Error("invalid event source chunk")
    if (notice.content.storage === "source" && (offset + bytes.length > notice.content.bytes || (result.next_offset === null) !== (offset + bytes.length === notice.content.bytes))) throw new Error("event source extent differs from notice")
    return result
  }
}
