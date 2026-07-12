import { writeStartupSplash } from "./startup"

type RuntimeBootstrap =
  | { readonly runtime: import("./runtime").TuiEngineRuntime | null; readonly error: null }
  | { readonly runtime: null; readonly error: unknown }

function markFirstPaint(): void {
  const marker = process.env.ROTTWEILER_FIRST_PAINT_MARKER
  if (marker === undefined || marker.length === 0) return
  const emittedAt =
    process.env.ROTTWEILER_FIRST_PAINT_EPOCH === "1"
      ? `:${performance.timeOrigin + performance.now()}`
      : ""
  process.stdout.write(`\n${marker}${emittedAt}\n`)
  delete process.env.ROTTWEILER_FIRST_PAINT_MARKER
  delete process.env.ROTTWEILER_FIRST_PAINT_EPOCH
}

async function main(): Promise<void> {
  // Keep OpenTUI and its native library behind the shipped startup paint.
  // A static import here would execute before this module body and make the
  // apparent splash wait on the native backend it is meant to cover.
  const { loadOpenTui } = await import("./opentui")
  const openTui = await loadOpenTui()
  let runtimeForShutdown: { stop(): Promise<void> } | null = null
  const renderer = await openTui.createCliRenderer({
    exitOnCtrlC: true,
    targetFps: 60,
    onDestroy: () => {
      // Closing the renderer must release the SSE/runtime handles so a normal
      // Ctrl+C can let the process end naturally. Never force exit 0 here:
      // OpenTUI also destroys after terminal/native setup failures, whose
      // original non-zero status must remain visible to the supervisor.
      void runtimeForShutdown?.stop()
    },
  })

  let resolveFirstFrame: (() => void) | undefined
  const firstFrame = new Promise<void>((resolve) => {
    resolveFirstFrame = resolve
  })
  let firstFrameMarked = false
  renderer.on(openTui.CliRenderEvents.FRAME, () => {
    if (firstFrameMarked) return
    firstFrameMarked = true
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
  const replaySession =
    process.env.ROTTWEILER_REPLAY_MODE === "1" ? configuredSession : undefined
  const keybindings = await parseKeybindingsFromEnvironment(
    process.env.ROTTWEILER_TUI_KEYBINDINGS,
  )
  const { daylightTheme, kennelTheme } = await import("./theme")
  const theme = process.env.ROTTWEILER_TUI_THEME === "daylight" ? daylightTheme : kennelTheme
  const terminalHandover = {
    suspend: () => renderer.suspend(),
    resume: () => renderer.resume(),
  }
  const app = appModule.createRottweilerApp(renderer, {
    ...(configuredSession === undefined || configuredSession.length === 0
      ? {}
      : { sessionId: configuredSession }),
    ...(replaySession === undefined || replaySession.length === 0
      ? {}
      : { replaySessionId: replaySession }),
    ...(keybindings === undefined ? {} : { keybindings }),
    theme,
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
      runtimeForShutdown = bootstrap.runtime
      bootstrap.runtime.bind(app)
      await bootstrap.runtime.start().catch(() => {
        // The runtime has already projected the actionable transport failure.
      })
    }
  })
}

async function parseKeybindingsFromEnvironment(source: string | undefined) {
  if (source === undefined || source.length === 0) return undefined
  // The Rust launcher may forward the TUI-only keybindings.toml section here;
  // keeping parsing local means extensions never leak into the engine protocol.
  const { parseKeybindingToml } = await import("./keybindings")
  return parseKeybindingToml(source)
}

writeStartupSplash(process.stdout)
markFirstPaint()
void main().catch((error: unknown) => {
  process.stderr.write(
    `rottweiler TUI failed to start: ${error instanceof Error ? error.message : "unknown error"}\n`,
  )
  process.exitCode = 1
})
