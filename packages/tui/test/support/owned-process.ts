import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { Worker } from "node:worker_threads"
import { registerTestProcess } from "./process-settlement"

interface ProcessResult { code: number; stdout: string; stderr: string }
interface ProcessOptions { cwd?: string; env?: Record<string, string>; timeoutMs: number; maxOutputBytes?: number }

/** A test directory remains owned until its Python supervisor proves group closure. */
export class TestProcessScope {
  readonly directory: string
  #pending: Promise<ProcessResult> | null = null
  #closing = false
  #unsettled = false

  private constructor(directory: string) { this.directory = directory }

  static async create(prefix: string): Promise<TestProcessScope> {
    return new TestProcessScope(await mkdtemp(join(tmpdir(), prefix)))
  }

  run(command: string[], options: ProcessOptions): Promise<ProcessResult> {
    if (this.#closing || this.#pending !== null || this.#unsettled) throw new Error("Test process scope is closed, busy, or UNSETTLED")
    const pending = this.#run(command, options)
    this.#pending = pending
    // Keep the actual physical promise reachable even if the test runner times out.
    void pending.finally(() => { if (this.#pending === pending) this.#pending = null }).catch(() => {})
    return pending
  }

  async #run(command: string[], options: ProcessOptions): Promise<ProcessResult> {
    const request = join(this.directory, "process.request.json")
    const result = join(this.directory, "process.result.json")
    const encoded = JSON.stringify({ command, cwd: options.cwd ?? process.cwd(), env: options.env ?? {},
      timeoutMs: options.timeoutMs, maxOutputBytes: options.maxOutputBytes ?? 1024 * 1024 })
    if (Buffer.byteLength(encoded) > 1024 * 1024) throw new Error("Oversized test process request")
    // One reusable result slot; no old acknowledgement may survive a new launch.
    await rm(result, { force: true })
    await writeFile(request, encoded)
    // This supervisor's own bounded deadline owns termination. Killing it could
    // abandon a native preload child; missing acknowledgement preserves evidence.
    this.#unsettled = true
    const registration = registerTestProcess()
    let protocolError: unknown = null
    // Bun's test VM automatically signals its subprocesses on test timeout,
    // including before Python installs its cooperative cancellation handler.
    // The owned worker VM keeps that startup handoff outside the test auto-killer.
    const code = await new Promise<number>((resolve) => {
      let bridgeCode = 125
      const worker = new Worker(new URL("./owned-process-worker.ts", import.meta.url), {
        workerData: { bridge: join(import.meta.dir, "owned-process.py"), request, result },
      })
      worker.on("message", (message: unknown) => {
        if (typeof message === "object" && message !== null && "started" in message &&
            typeof message.started === "number" && Number.isInteger(message.started) && message.started > 0) {
          try { registration.started(message.started) } catch (error) { protocolError = error }
        }
        if (typeof message === "number" && Number.isInteger(message)) bridgeCode = message
      })
      worker.on("error", () => { bridgeCode = 125 })
      worker.on("exit", (status) => resolve(status === 0 ? bridgeCode : 125))
    })
    if (protocolError !== null) throw new Error(`UNSETTLED test registration; retained ${this.directory}`, { cause: protocolError })
    let payload: unknown
    try {
      if ((await stat(result)).size > 6 * 1024 * 1024 + 8192) throw new Error("Missing bounded acknowledgement")
      payload = JSON.parse(await readFile(result, "utf8"))
      if (code !== 0 || typeof payload !== "object" || payload === null || !("settled" in payload) || payload.settled !== true) {
        const detail = typeof payload === "object" && payload !== null && "error" in payload && typeof payload.error === "string"
          ? payload.error.slice(0, 24 * 1024) : "Missing physical-settlement acknowledgement"
        throw new Error(detail)
      }
    } catch (error) {
      throw new Error(`UNSETTLED test process; retained ${this.directory}: ${String(error)}`, { cause: error })
    }
    registration.settled()
    this.#unsettled = false
    if ("error" in payload) throw new Error(String(payload.error))
    if (!("code" in payload) || !Number.isInteger(payload.code) || !("stdout" in payload) || typeof payload.stdout !== "string" ||
        !("stderr" in payload) || typeof payload.stderr !== "string") throw new Error("Invalid test process result")
    return { code: payload.code as number, stdout: payload.stdout, stderr: payload.stderr }
  }

  async close(): Promise<void> {
    this.#closing = true
    // Test timeout cleanup waits the same physical owner; no competing kill path.
    await this.#pending?.catch(() => {})
    if (this.#unsettled) throw new Error(`UNSETTLED test process; retained ${this.directory}`)
    await rm(this.directory, { recursive: true, force: true })
  }
}
