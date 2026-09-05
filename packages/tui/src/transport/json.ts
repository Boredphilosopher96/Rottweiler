/** A response body owns one amortized buffer; fragmented input cannot grow a chunk array. */
export async function boundedJson(response: Response, maxBytes: number): Promise<unknown> {
  const declared = response.headers.get("content-length")
  if (declared !== null && Number(declared) > maxBytes) throw new Error("reply exceeds its byte limit")
  if (response.body === null) throw new Error("reply has no body")
  const reader = response.body.getReader()
  let buffer = new Uint8Array(Math.min(4096, maxBytes))
  let length = 0
  try {
    for (;;) {
      const result = await reader.read()
      if (result.done) break
      if (result.value.byteLength > maxBytes - length) throw new Error("reply exceeds its byte limit")
      const needed = length + result.value.byteLength
      if (needed > buffer.byteLength) {
        const grown = new Uint8Array(Math.min(maxBytes, Math.max(needed, buffer.byteLength * 2)))
        grown.set(buffer.subarray(0, length))
        buffer = grown
      }
      buffer.set(result.value, length)
      length = needed
    }
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(buffer.subarray(0, length)))
  } catch (error) {
    await reader.cancel(error).catch(() => undefined)
    throw error
  } finally {
    reader.releaseLock()
  }
}
