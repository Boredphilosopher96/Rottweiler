import { parentPort, workerData } from "node:worker_threads"

// This worker only owns the Python supervisor. Python owns its bounded native
// process group; do not terminate the worker while that physical work is pending.
const { bridge, request, result } = workerData as { bridge: string; request: string; result: string }
const child = Bun.spawn(["python3", bridge, request, result], {
  stdin: "ignore", stdout: "ignore", stderr: "ignore",
})
parentPort!.postMessage(await child.exited)
parentPort!.close()
