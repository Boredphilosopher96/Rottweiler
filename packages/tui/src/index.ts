import { writeStartupSplash } from "./startup"
import { enhancedKeyboardOptions } from "./keybindings"
import { observedResidentBytes } from "./process-memory"
import type { ClientStage } from "./client-diagnostics"
import {
  registerTreeSitterParsersLazily,
  stabilizeTreeSitterClient,
} from "./tree-sitter-client"

type RuntimeBootstrap =
  | {
      readonly runtime: import("./runtime").TuiEngineRuntime | null
      readonly error: null
    }
  | { readonly runtime: null; readonly error: unknown }

function emitStartupMarker(markerVariable: string, epochVariable?: string): void {
  const marker = process.env[markerVariable]
  if (marker === undefined || marker.length === 0) return
  const emittedAt =
    epochVariable !== undefined && process.env[epochVariable] === "1"
      ? `:${performance.timeOrigin + performance.now()}`
      : ""
  process.stdout.write(`\n${marker}${emittedAt}\n`)
  delete process.env[markerVariable]
  if (epochVariable !== undefined) delete process.env[epochVariable]
}

async function main(): Promise<void> {
  const diagnostics = process.env.ROTTWEILER_CLIENT_TIMINGS === "1"
    ? new (await import("./client-diagnostics")).ClientDiagnostics() : undefined
  let startupStageStarted = diagnostics?.start() ?? 0
  const finishStartupStage = (stage: ClientStage): void => {
    if (diagnostics === undefined) return
    diagnostics.finish(stage, startupStageStarted)
    startupStageStarted = diagnostics.start()
  }
  const expectedSupervisorPid = Number.parseInt(
    process.env.ROTTWEILER_SUPERVISOR_PID ?? "",
    10,
  )
  const recycleStatePath = process.env.ROTTWEILER_TUI_RECYCLE_STATE_FILE
  let supervisorDeathTimer: ReturnType<typeof setInterval> | undefined
  if (Number.isSafeInteger(expectedSupervisorPid) && expectedSupervisorPid > 1) {
    supervisorDeathTimer = setInterval(() => {
      if (process.ppid === expectedSupervisorPid) return
      process.exit(143)
    }, 100)
    supervisorDeathTimer.unref()
  }
  // Keep OpenTUI and its native library behind the shipped startup paint.
  // A static import here would execute before this module body and make the
  // apparent splash wait on the native backend it is meant to cover.
  const { loadOpenTui } = await import("./opentui")
  const openTui = await loadOpenTui()
  finishStartupStage("startup_modules")
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
  let treeSitterParsers: ReturnType<
    typeof import("./tree-sitter-runtime").embeddedParserConfigurations
  > = []
  let exitRequested = false
  let rssRecycleTimer: ReturnType<typeof setInterval> | undefined
  const renderer = await openTui.createCliRenderer({
    exitOnCtrlC: true,
    targetFps: 60,
    // Extended keyboard events keep macOS Command+Arrow distinct from Ctrl+E,
    // so terminal navigation can never masquerade as the external-editor key.
    useKittyKeyboard: enhancedKeyboardOptions,
    onDestroy: () => {
      if (diagnostics !== undefined) process.stderr.write(`[rw-client-timings] ${JSON.stringify(diagnostics.snapshot())}\n`)
      if (rssRecycleTimer !== undefined) clearInterval(rssRecycleTimer)
      if (supervisorDeathTimer !== undefined) clearInterval(supervisorDeathTimer)
      // Closing the renderer must release the SSE/runtime handles so a normal
      // Ctrl+C can let the process end naturally. Never force exit 0 here:
      // OpenTUI also destroys after terminal/native setup failures, whose
      // original non-zero status must remain visible to the supervisor.
      void runtimeForShutdown?.stop()
      void (async () => {
        await openTui.destroyTreeSitterClient()
        await treeSitterRuntime?.cleanup()
        treeSitterRuntime = null
        delete process.env.OTUI_ASSET_ROOT
        delete process.env.OTUI_TREE_SITTER_WORKER_PATH
      })()
    },
  })

  finishStartupStage("startup_renderer")
  let resolveFirstFrame: (() => void) | undefined
  const firstFrame = new Promise<void>((resolve) => {
    resolveFirstFrame = resolve
  })
  let firstFrameMarked = false
  let appMounted = false
  let transcriptPainted = false
  let composerAcceptedInput = false
  let interactiveMarked = false
  renderer.on(openTui.CliRenderEvents.FRAME, () => {
    if (!firstFrameMarked) {
      firstFrameMarked = true
      resolveFirstFrame?.()
    }
    if (!appMounted) return
    if (!transcriptPainted) {
      transcriptPainted = true
      finishStartupStage("startup_paint")
      emitStartupMarker("ROTTWEILER_TRANSCRIPT_PAINTED_MARKER")
    }
    if (!composerAcceptedInput || interactiveMarked) return
    interactiveMarked = true
    finishStartupStage("startup_input")
    if (diagnostics !== undefined) {
      const stages = diagnostics.snapshot().stages.filter(({ stage }) => stage.startsWith("startup_"))
      process.stderr.write(`[rw-startup-timings] ${JSON.stringify(stages)}\n`)
    }
    emitStartupMarker(
      "ROTTWEILER_INTERACTIVE_MARKER",
      "ROTTWEILER_INTERACTIVE_EPOCH",
    )
  })

  const startupFrame = new openTui.TextRenderable(renderer, {
    content: "◆ Rottweiler\n  waking the engine…",
    height: 2,
    width: "100%",
  })
  renderer.root.add(startupFrame)
  await firstFrame
  finishStartupStage("startup_first_frame")

  const [appModule, platform, runtimeModule, stateModule] = await Promise.all([
    import("./app"),
    import("./platform"),
    import("./runtime"),
    import("./state"),
  ])
  finishStartupStage("startup_app_modules")
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
    diagnostics,
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
  finishStartupStage("startup_configuration")
  // OpenTUI workers require real filesystem paths. Bun embeds the selected
  // parser assets inside the executable; materialize a private, bounded runtime
  // after first paint and remove it when the renderer shuts down.
  try {
    const { embeddedParserConfigurations, materializeTreeSitterRuntime } = await import("./tree-sitter-runtime")
    treeSitterRuntime = await materializeTreeSitterRuntime()
    const { assetsPath, root, workerPath } = treeSitterRuntime
    process.env.OTUI_ASSET_ROOT = root
    process.env.OTUI_TREE_SITTER_WORKER_PATH = workerPath
    treeSitterParsers = embeddedParserConfigurations(assetsPath)
    openTui.addDefaultParsers(
      treeSitterParsers.filter(
        ({ filetype }) => filetype === "markdown" || filetype === "markdown_inline",
      ),
    )
  } catch {
    // Markdown remains readable if a locked-down host cannot create the
    // ephemeral parser runtime. Never fail application startup for highlighting.
  }
  const treeSitterClient = stabilizeTreeSitterClient(
    registerTreeSitterParsersLazily(
      openTui.getTreeSitterClient(),
      treeSitterParsers,
    ),
  )
  finishStartupStage("startup_parser_assets")
  void treeSitterClient.initialize().catch(() => {
    // Markdown remains readable if a terminal cannot start a worker. OpenTUI
    // reports the parser failure; the application must stay usable.
  })
  const terminalHandover = createTerminalHandover(renderer)
  const app = appModule.createRottweilerApp(renderer, {
    diagnostics,
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
    onComposerInput: (value) => {
      if (transcriptPainted && value.length > 0) composerAcceptedInput = true
    },
    historyReader: {
      page: async (sessionId, read, signal) => {
        const { runtime } = await runtimeBootstrap
        if (runtime === null) throw new Error("engine runtime is unavailable")
        return runtime.historyReader.page(sessionId, read, signal)
      },
      content: async (sessionId, read, signal) => {
        const { runtime } = await runtimeBootstrap
        if (runtime === null) throw new Error("engine runtime is unavailable")
        return runtime.historyReader.content(sessionId, read, signal)
      },
    },
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
  const { readTuiRecycleState, recycleTuiIfNeeded } = await import("./recycle-state")
  const recycledState = readTuiRecycleState(recycleStatePath)
  if (recycledState !== null) {
    app.restoreRecycleState(recycledState)
    renderer.on(openTui.CliRenderEvents.FRAME, () => app.applyPendingRecycleScroll())
  }
  startupFrame.destroy()
  renderer.root.add(app)
  appMounted = true
  finishStartupStage("startup_app_mount")
  // OpenTUI's native allocator can retain released render graphs during very
  // long tool-heavy sessions. The host already supervises this private TUI and
  // replays durable state after an exit, so recycle before allocator residency
  // can breach the release RSS envelope. This is process-local: the engine,
  // session, transcript, and active turn remain owned by the host.
  // The nightly envelope measures the engine and TUI together. Leaving roughly
  // 200 MiB for the engine keeps the combined pair comfortably below 600 MiB.
  const tuiRssRecycleBytes = 384 * 1024 * 1024
  let nextRecycleAttemptAt = 0
  rssRecycleTimer = setInterval(() => {
    const observedRss = observedResidentBytes()
    if (exitRequested || observedRss < tuiRssRecycleBytes) return
    if (Date.now() < nextRecycleAttemptAt) return
    nextRecycleAttemptAt = Date.now() + 10_000
    recycleTuiIfNeeded({
      observedBytes: observedRss,
      thresholdBytes: tuiRssRecycleBytes,
      path: recycleStatePath,
      capture: () => app.recycleState(),
      recycle: () => {
        exitRequested = true
        process.exitCode = 75
        renderer.destroy()
      },
    })
  }, 100)
  rssRecycleTimer.unref()
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
emitStartupMarker("ROTTWEILER_PROCESS_START_MARKER", "ROTTWEILER_PROCESS_START_EPOCH")
void main().catch((error: unknown) => {
  process.stderr.write(
    `rottweiler TUI failed to start: ${error instanceof Error ? error.message : "unknown error"}\n`,
  )
  process.exitCode = 1
})
