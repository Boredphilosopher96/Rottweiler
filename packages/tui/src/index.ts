import { writeStartupSplash } from "./startup"
import { enhancedKeyboardOptions } from "./keybindings"

type RuntimeBootstrap =
  | {
      readonly runtime: import("./runtime").TuiEngineRuntime | null
      readonly error: null
    }
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
  const treeSitterSmokeReport = process.env.ROTTWEILER_TREE_SITTER_SMOKE_REPORT
  if (treeSitterSmokeReport !== undefined && treeSitterSmokeReport.length > 0) {
    const { runCompiledTreeSitterSmoke } = await import("./tree-sitter-smoke")
    await runCompiledTreeSitterSmoke(treeSitterSmokeReport)
    return
  }
  let runtimeForShutdown: {
    shutdownHost(): Promise<boolean>
    stop(): Promise<void>
  } | null = null
  let treeSitterRuntime: import("./tree-sitter-runtime").MaterializedTreeSitterRuntime | null = null
  let exitRequested = false
  const renderer = await openTui.createCliRenderer({
    exitOnCtrlC: true,
    targetFps: 60,
    // Extended keyboard events keep macOS Command+Arrow distinct from Ctrl+E,
    // so terminal navigation can never masquerade as the external-editor key.
    useKittyKeyboard: enhancedKeyboardOptions,
    onDestroy: () => {
      // Closing the renderer must release the SSE/runtime handles so a normal
      // Ctrl+C can let the process end naturally. Never force exit 0 here:
      // OpenTUI also destroys after terminal/native setup failures, whose
      // original non-zero status must remain visible to the supervisor.
      void runtimeForShutdown?.stop()
      void (async () => {
        await openTui.destroyTreeSitterClient()
        await treeSitterRuntime?.cleanup()
        treeSitterRuntime = null
      })()
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
    createExternalUrlAdapter,
    createImagePasteAdapter,
    createTerminalHandover,
    createTextClipboardAdapter,
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
  const replaySession = process.env.ROTTWEILER_REPLAY_MODE === "1" ? configuredSession : undefined
  const keybindings = await parseKeybindingsFromEnvironment(process.env.ROTTWEILER_TUI_KEYBINDINGS)
  const { homedir } = await import("node:os")
  const { join: joinPath } = await import("node:path")
  const {
    kennelTheme,
    loadCustomThemes,
    systemThemeFor,
    systemThemeFromPalette,
    themeByName,
  } = await import("./theme")
  await loadCustomThemes(joinPath(homedir(), ".rottweiler", "themes"))
  const configuredTheme = process.env.ROTTWEILER_TUI_THEME ?? ""
  const terminalThemeMode = renderer.themeMode ?? "dark"
  const terminalPalette = renderer.getPalette({ size: 16, timeout: 250 }).catch(() => null)
  const theme = configuredTheme === "system"
    ? systemThemeFor(terminalThemeMode)
    : themeByName(configuredTheme, terminalThemeMode) ?? themeByName("opencode", terminalThemeMode) ?? kennelTheme
  // OpenTUI workers require real filesystem paths. Bun embeds the selected
  // parser assets inside the executable; materialize a private, bounded runtime
  // after first paint and remove it when the renderer shuts down.
  try {
    const { embeddedParserConfigurations, materializeTreeSitterRuntime } = await import("./tree-sitter-runtime")
    treeSitterRuntime = await materializeTreeSitterRuntime()
    const { assetsPath, workerPath } = treeSitterRuntime
    process.env.OTUI_TREE_SITTER_WORKER_PATH = workerPath
    openTui.addDefaultParsers(embeddedParserConfigurations(assetsPath))
  } catch {
    // Markdown remains readable if a locked-down host cannot create the
    // ephemeral parser runtime. Never fail application startup for highlighting.
  }
  const treeSitterClient = openTui.getTreeSitterClient()
  void treeSitterClient.initialize().catch(() => {
    // Markdown remains readable if a terminal cannot start a worker. OpenTUI
    // reports the parser failure; the application must stay usable.
  })
  const terminalHandover = createTerminalHandover(renderer)
  const app = appModule.createRottweilerApp(renderer, {
    ...(configuredSession === undefined || configuredSession.length === 0
      ? {}
      : { sessionId: configuredSession }),
    ...(replaySession === undefined || replaySession.length === 0
      ? {}
      : { replaySessionId: replaySession }),
    ...(keybindings === undefined ? {} : { keybindings }),
    theme,
    systemThemeMode: terminalThemeMode,
    treeSitterClient,
    onCommand: async (command) => {
      const bootstrap = await runtimeBootstrap
      return (await bootstrap.runtime?.sendCommand(command)) ?? null
    },
    onProviderApiKey: async (provider, apiKey) => {
      const bootstrap = await runtimeBootstrap
      if (bootstrap.runtime === null) throw new Error("engine runtime is unavailable")
      return await bootstrap.runtime.submitProviderApiKey(provider, apiKey)
    },
    onProviderActivate: async (provider) => {
      const bootstrap = await runtimeBootstrap
      if (bootstrap.runtime === null) throw new Error("engine runtime is unavailable")
      await bootstrap.runtime.activateProvider(provider)
    },
    onSessionSelect: async (sessionId) => {
      const bootstrap = await runtimeBootstrap
      await bootstrap.runtime?.switchSession(sessionId)
    },
    terminalHandover,
    editor: createExternalEditorAdapter(terminalHandover),
    externalUrl: createExternalUrlAdapter(),
    notifications: createDesktopNotificationAdapter(),
    imagePaste: createImagePasteAdapter(),
    textClipboard: createTextClipboardAdapter(),
    // Let Composer finish clearing the accepted slash command, then ask the
    // authenticated host to stop before releasing the renderer. Process exit
    // remains a bounded supervisor fallback when the control plane is down.
    onExit: () => {
      if (exitRequested) return
      exitRequested = true
      queueMicrotask(() => {
        void (async () => {
          const runtime = runtimeForShutdown ?? (await runtimeWithin(runtimeBootstrap, 250))
          await runtime?.shutdownHost()
          renderer.destroy()
        })()
      })
    },
  })
  startupFrame.destroy()
  renderer.root.add(app)
  void terminalPalette.then((colors) => {
    if (colors === null) return
    app.setSystemTheme(systemThemeFromPalette(colors, terminalThemeMode))
  })

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

async function runtimeWithin(
  bootstrap: Promise<RuntimeBootstrap>,
  timeoutMs: number,
): Promise<import("./runtime").TuiEngineRuntime | null> {
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    const result = await Promise.race([
      bootstrap,
      new Promise<null>((resolve) => {
        timer = setTimeout(() => resolve(null), timeoutMs)
      }),
    ])
    return result === null ? null : result.runtime
  } finally {
    if (timer !== undefined) clearTimeout(timer)
  }
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
