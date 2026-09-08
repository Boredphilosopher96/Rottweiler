import { expect, spyOn, test } from "bun:test"
import { PROTOCOL_LIMITS } from "../src/generated/protocol-3"
import { StreamCredit } from "../src/stream-credit"
import { BoundedJsonWriter, LineTooLargeError, OutboundQueueFullError } from "../src/transport"

test("provider credit debits the single encoded snapshot while control remains live", async () => {
  const output: string[] = []
  const writer = new BoundedJsonWriter({ write(bytes) { output.push(new TextDecoder().decode(bytes)) } })
  const credit = new StreamCredit(new AbortController().signal)
  let reads = 0
  const pending = writer.write({ get text() { reads += 1; return reads === 1 ? "😀" : "changed" } }, "data", credit)
  expect(reads).toBe(1)
  let drained = false
  const drain = writer.drain().then(() => { drained = true })
  await writer.write({ result: "control" })
  expect(output).toEqual(['{"result":"control"}\n'])
  expect(drained).toBe(false)
  credit.grant(1, Buffer.byteLength('{"text":"😀"}'))
  await pending
  await drain
  expect(output).toEqual(['{"result":"control"}\n', '{"text":"😀"}\n'])
  expect(reads).toBe(1)
  credit.close()
})

test("provider window bounds construction before encoded allocation or credit wait", async () => {
  const encode = spyOn(TextEncoder.prototype, "encodeInto")
  const credit = new StreamCredit(new AbortController().signal)
  let visited = false
  try {
    const writer = new BoundedJsonWriter({ write() { throw new Error("not admitted") } }, PROTOCOL_LIMITS.providerWindowBytes + 1024)
    await expect(writer.write({ text: "\u0000".repeat(PROTOCOL_LIMITS.providerWindowBytes / 2),
      get later() { visited = true; return null } }, "data", credit)).rejects.toBeInstanceOf(LineTooLargeError)
    expect(visited).toBe(false)
    expect(encode).not.toHaveBeenCalled()
  } finally { encode.mockRestore(); credit.close() }
})

test("credit-blocked frames consume shared queue slots before more getters run", async () => {
  const writer = new BoundedJsonWriter({ write() { throw new Error("no credit") } })
  const credits = Array.from({ length: PROTOCOL_LIMITS.maxProviderStreams }, () => new StreamCredit(new AbortController().signal))
  const pending = credits.map(credit => writer.write(null, "data", credit).catch(error => error))
  let visited = false
  await expect(writer.write({ get value() { visited = true; return null } }, "data")).rejects.toBeInstanceOf(OutboundQueueFullError)
  expect(visited).toBe(false)
  for (const error of await Promise.all(pending)) expect(error).toBeInstanceOf(OutboundQueueFullError)
  await expect(writer.drain()).rejects.toBeInstanceOf(OutboundQueueFullError)
  for (const credit of credits) await expect(credit.take(1)).rejects.toBeInstanceOf(OutboundQueueFullError)
})

test("cancelled credit releases its retained frame and settles drain without poisoning output", async () => {
  const output: string[] = []
  const writer = new BoundedJsonWriter({ write(bytes) { output.push(new TextDecoder().decode(bytes)) } })
  const controller = new AbortController()
  const credit = new StreamCredit(controller.signal)
  const pending = writer.write("cancelled", "data", credit).catch(error => error)
  const drain = writer.drain()
  controller.abort()
  expect(await pending).toBeInstanceOf(Error)
  await drain
  await writer.write("live")
  expect(output).toEqual(['"live"\n'])
})
