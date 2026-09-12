import { JsonAllocationShape, type ReplyAllocation } from "./reply-allocation"
import { EngineProtocolError } from "./errors"
import type { ClientDiagnostics } from "../client-diagnostics"
/** A response body owns one amortized buffer; fragmented input cannot grow a chunk array. */
export async function boundedJson(response: Response, maxBytes: number, diagnostics?: ClientDiagnostics, signal?: AbortSignal, allocation?: ReplyAllocation): Promise<unknown> {
  signal?.throwIfAborted()
  const declared = response.headers.get("content-length")
  if (declared !== null && Number(declared) > maxBytes) {
    await response.body?.cancel()
    throw new EngineProtocolError("reply exceeds its byte limit")
  }
  if (response.body === null) throw new EngineProtocolError("reply has no body")
  const reader = response.body.getReader()
  const shape = allocation === undefined ? null : new JsonAllocationShape()
  let buffer = new Uint8Array(0)
  let length = 0
  try {
    allocation?.admit(shape!.peak(Math.min(4096, maxBytes), 0))
    buffer = new Uint8Array(Math.min(4096, maxBytes))
    for (;;) {
      const result = await reader.read()
      signal?.throwIfAborted()
      if (result.done) break
      if (result.value.byteLength > maxBytes - length) throw new EngineProtocolError("reply exceeds its byte limit")
      const needed = length + result.value.byteLength
      const accountingAt = shape === null ? undefined : diagnostics?.start()
      try { shape?.append(result.value) }
      finally { if (accountingAt !== undefined) diagnostics?.finish("reply_allocation", accountingAt, result.value.byteLength) }
      const capacity = needed > buffer.byteLength ? Math.min(maxBytes, Math.max(needed, buffer.byteLength * 2)) : buffer.byteLength
      allocation?.admit(shape!.peak(capacity, capacity === buffer.byteLength ? 0 : buffer.byteLength))
      if (needed > buffer.byteLength) {
        const grown = new Uint8Array(Math.min(maxBytes, Math.max(needed, buffer.byteLength * 2)))
        grown.set(buffer.subarray(0, length))
        buffer = grown
      }
      buffer.set(result.value, length)
      length = needed
    }
    if (declared !== null && response.headers.get("content-encoding") === null
      && Number(declared) !== length) throw new EngineProtocolError("reply length does not match Content-Length")
    const startedAt = diagnostics?.start()
    try {
      return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(buffer.subarray(0, length)))
    } catch {
      throw new EngineProtocolError("reply must contain valid UTF-8 JSON")
    } finally {
      if (startedAt !== undefined) diagnostics?.finish("reply_decode", startedAt, length)
    }
  } catch (error) {
    await reader.cancel(error).catch(() => undefined)
    signal?.throwIfAborted()
    throw error
  } finally {
    reader.releaseLock()
  }
}
