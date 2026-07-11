import { writeStartupSplash } from "./startup"
import { loadOpenTui } from "./opentui"

type RuntimeBootstrap =
  | { readonly runtime: import("./runtime").TuiEngineRuntime | null; readonly error: null }
  | { readonly runtime: null; readonly error: unknown }

async function main(): Promise<void> {
  const openTui = await loadOpenTui()
  const renderer = await openTui.createCliRenderer({
    exitOnCtrlC: true,
    targetFps: 60,
  })

  let resolveFirstFrame: (() => void) | undefined
  const firstFrame = new Promise<void>((resolve) => {
    resolveFirstFrame = resolve
  })
  let firstFrameMarked = false
  const firstFrameMarker = process.env.ROTTWEILER_FIRST_FRAME_MARKER
  renderer.on(openTui.CliRenderEvents.FRAME, () => {
    if (firstFrameMarked) return
    firstFrameMarked = true
    if (firstFrameMarker !== undefined && firstFrameMarker.length > 0) {
      process.stdout.write(`\n${firstFrameMarker}\n`)
      delete process.env.ROTTWEILER_FIRST_FRAME_MARKER
    }
    resolveFirstFrame?.()
  })

  const startupFrame = new openTui.TextRenderable(renderer, {
    content: "◆ Rottweiler\n  waking the engine…",
    height: 2,
    width: "100%",
  })
  renderer.root.add(startupFrame)
  await firstFrame

  const [appModule, platform, runtimeModule, stateModule] = await Promise.all([
    import("./app"),
    import("./platform"),
    import("./runtime"),
    import("./state"),
  ])
  const {
    createDesktopNotificationAdapter,
    createExternalEditorAdapter,
    createImagePasteAdapter,
  } = platform
  const { createEngineRuntimeFromEnvironment } = runtimeModule
  const { reduceRottweilerState, transportDisconnected } = stateModule

  const runtimeBootstrap: Promise<RuntimeBootstrap> = createEngineRuntimeFromEnvironment({
    onDriverReady: () => {
      const marker = process.env.ROTTWEILER_DRIVER_READY_MARKER
      if (marker !== undefined && marker.length > 0) {
        process.stdout.write(`\n${marker}\n`)
        delete process.env.ROTTWEILER_DRIVER_READY_MARKER
      }
    },
  }).then(
    (runtime) => ({ runtime, error: null }),
    (error: unknown) => ({ runtime: null, error }),
  )

  const configuredSession = process.env.ROTTWEILER_SESSION_ID
  const terminalHandover = {
    suspend: () => renderer.suspend(),
    resume: () => renderer.resume(),
  }
  const app = appModule.createRottweilerApp(renderer, {
    ...(configuredSession === undefined || configuredSession.length === 0
      ? {}
      : { sessionId: configuredSession }),
    onCommand: async (command) => {
      const bootstrap = await runtimeBootstrap
      return (await bootstrap.runtime?.sendCommand(command)) ?? null
    },
    onSessionSelect: async (sessionId) => {
      const bootstrap = await runtimeBootstrap
      await bootstrap.runtime?.switchSession(sessionId)
    },
    terminalHandover,
    editor: createExternalEditorAdapter(terminalHandover),
    notifications: createDesktopNotificationAdapter(),
    imagePaste: createImagePasteAdapter(),
  })
  startupFrame.destroy()
  renderer.root.add(app)

  void runtimeBootstrap.then(async (bootstrap) => {
    if (bootstrap.error !== null) {
      app.setState(
        reduceRottweilerState(
          app.state,
          transportDisconnected(
            0,
            bootstrap.error instanceof Error
              ? bootstrap.error.message
              : "engine runtime initialization failed",
          ),
        ),
      )
      return
    }
    if (bootstrap.runtime !== null) {
      bootstrap.runtime.bind(app)
      await bootstrap.runtime.start().catch(() => {
        // The runtime has already projected the actionable transport failure.
      })
    }
  })
}

writeStartupSplash(process.stdout)
void main().catch((error: unknown) => {
  process.stderr.write(
    `rottweiler TUI failed to start: ${error instanceof Error ? error.message : "unknown error"}\n`,
  )
  process.exitCode = 1
})
