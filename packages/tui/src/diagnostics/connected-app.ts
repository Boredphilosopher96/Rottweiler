import type { ClientCommand, EngineEvent } from "../protocol"
import { createRottweilerApp } from "../app"
import { ClientDiagnostics } from "../client-diagnostics"
import { createEngineRuntimeFromEnvironment } from "../runtime"
import type { ConnectedInput } from "./connected-input"
import { createMemoryRenderer } from "./memory-renderer"

/** The normal authenticated runtime consumes every command, read and SSE event. */
export async function connectedApp(input: ConnectedInput, observeCommand: (command: ClientCommand) => void = () => {}, closeHttpRequests = false, observeEvent: (event: EngineEvent) => void = () => {}) {
  const native = await createMemoryRenderer()
  try { return await bindConnectedApp(native, input, observeCommand, closeHttpRequests, observeEvent) }
  catch (error) { native.setup.renderer.destroy(); throw error }
}

async function bindConnectedApp(native: Awaited<ReturnType<typeof createMemoryRenderer>>, input: ConnectedInput, observeCommand: (command: ClientCommand) => void, closeHttpRequests: boolean, observeEvent: (event: EngineEvent) => void) {
  const diagnostics = new ClientDiagnostics()
  let readySession: string | undefined
  const fixtureFetch = closeHttpRequests ? ((request, options) => {
    const headers = new Headers(options?.headers)
    headers.set("connection", "close")
    return fetch(request, { ...options, headers })
  }) as typeof fetch : undefined
  const runtime = await createEngineRuntimeFromEnvironment({ diagnostics, ...(fixtureFetch === undefined ? {} : { fetch: fixtureFetch }), environment: {
    ROTTWEILER_ENGINE_SOCKET: input.socketPath,
    ROTTWEILER_ENGINE_TOKEN_FILE: input.bootstrapTokenFile,
    ROTTWEILER_SESSION_ID: input.sessionId,
  }, onDriverReady: session => { readySession = session } })
  requireThat(runtime !== null, "connected probe requires engine runtime")
  let app: ReturnType<typeof createRottweilerApp>
  try { app = createRottweilerApp(native.setup.renderer, { sessionId: input.sessionId,
    sessionReader: runtime.sessionReader, familyControls: runtime.familyControls,
    allocations: runtime.allocations, diagnostics, treeSitterClient: native.treeSitter,
    onCommand: (command, allocation) => { observeCommand(command); return runtime.sendCommand(command, allocation) },
  }) } catch (error) { await runtime.stop(); throw error }
  try {
  native.setup.renderer.root.add(app)
  runtime.bind({
    get historyCache() { return app.historyCache }, get state() { return app.state },
    installBootstrap: value => app.installBootstrap(value),
    handleEvent: event => { observeEvent(event); app.handleEvent(event) },
    setState: state => app.setState(state), setSessionId: id => app.setSessionId(id),
    beginInitialReplayBatch: () => app.beginInitialReplayBatch(), endInitialReplayBatch: () => app.endInitialReplayBatch(),
    resetConnectionProjections: () => app.resetConnectionProjections(),
  })
  } catch (error) {
    try { await runtime.stop() } finally { app.destroy() }
    throw error
  }
  let runtimeFailed = false
  let runtimeFailure: unknown
  const running = runtime.start().catch(error => { runtimeFailed = true; runtimeFailure = error })
  const until = async (label: string, predicate: () => boolean, timeoutMs = 10_000) => {
    const deadline = performance.now() + timeoutMs
    while (!predicate()) {
      if (runtimeFailed) throw runtimeFailure
      requireThat(performance.now() < deadline, `connected probe waiting for ${label}; connection=${app.state.connection.phase}`)
      await Bun.sleep(1)
      await native.setup.renderOnce()
    }
    await native.setup.renderOnce()
  }
  let closing: Promise<void> | undefined
  const close = () => closing ??= (async () => {
    const errors: unknown[] = []
    try { await runtime.stop() } catch (error) { errors.push(error) }
    try { await running } catch (error) { errors.push(error) }
    if (runtimeFailed) errors.push(runtimeFailure)
    try { app.destroy() } catch (error) { errors.push(error) }
    try { native.setup.renderer.destroy() } catch (error) { errors.push(error) }
    const deadline = performance.now() + 10_000
    while (runtime.allocations.usage.bytes !== 0 && performance.now() < deadline) await Bun.sleep(1)
    if (runtime.allocations.usage.bytes !== 0) errors.push(new Error("connected probe allocation owner did not retire"))
    if (native.terminal.snapshot.queuedBytes !== 0) errors.push(new Error("connected native sink did not drain"))
    if (errors.length) throw new AggregateError(errors, "connected probe cleanup failed")
  })()
  return { ...native, app, runtime, diagnostics, until, close,
    ready: () => readySession === input.sessionId && app.state.connection.phase === "connected"
      && app.composer.editor.focused }
}

export function requireThat(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message)
}
