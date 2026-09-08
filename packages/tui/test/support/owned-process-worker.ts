import { parentPort, workerData } from "node:worker_threads"

// This worker only owns the Python supervisor. Python owns its bounded native
// process group. The stdin pipe keeps parent lifetime explicit: VM death closes
// it and requests cooperative cancellation in Python without killing its owner.
const { bridge, request, result } = workerData as { bridge: string; request: string; result: string }
const child = Bun.spawn(["python3", bridge, request, result], {
  stdin: "pipe", stdout: "ignore", stderr: "ignore",
})
const status = await child.exited
await child.stdin.end()
parentPort!.postMessage(status)
parentPort!.close()
