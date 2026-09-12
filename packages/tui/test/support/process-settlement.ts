import { fstatSync, writeSync } from "node:fs"

/** The direct test VM acknowledges only physically retired subprocess scopes. */
const inherited = process.env.RW_PERF_SETTLEMENT_FD
delete process.env.RW_PERF_SETTLEMENT_FD
const descriptor = inherited === undefined ? null : Number(inherited)
if (descriptor !== null && (!Number.isInteger(descriptor) || descriptor < 3 || !fstatSync(descriptor).isFIFO())) {
  throw new Error("Invalid test process settlement pipe")
}
const active = new Set<string>()
let counter = 0
let failed = false
let closed = false

function send(message: object): void {
  if (descriptor === null) return
  const bytes = Buffer.from(`${JSON.stringify(message)}\n`)
  try {
    // The Python owner supplies a nonblocking pipe. A full/missing reader is a
    // failed proof; never buffer arbitrary protocol output or block the test VM.
    if (bytes.byteLength > 512 || writeSync(descriptor, bytes) !== bytes.byteLength) {
      throw new Error("Test settlement record was not atomic")
    }
  } catch (error) {
    failed = true
    throw error
  }
}

export function closeTestProcesses(): void {
  if (closed) return
  if (failed || active.size !== 0) throw new Error(`UNSETTLED test process scopes: ${[...active].join(",")}`)
  send({ kind: "closed" })
  closed = true
}

process.once("exit", () => {
  if (!failed && active.size === 0) {
    try { closeTestProcesses() } catch { /* missing acknowledgement fails the outer owner */ }
  }
})

export function registerTestProcess(): { started(pid: number): void; settled(): void } {
  if (closed || failed || active.size >= 128) throw new Error("Test process settlement scope unavailable")
  const token = `${process.pid}:${++counter}`
  active.add(token)
  send({ kind: "starting", token })
  return {
    started(pid) { send({ kind: "started", token, pid }) },
    settled() {
      if (!active.delete(token)) throw new Error("Test process was already settled")
      send({ kind: "settled", token })
    },
  }
}
