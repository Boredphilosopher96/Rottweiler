import type { ReplyAllocation } from "../transport/reply-allocation"
import { type ThemeMode, type TreeSitterClient } from "@opentui/core"
import type { ClientDiagnostics } from "../client-diagnostics"
import type { SessionReader } from "../session-reader"
import { type KeybindingConfiguration } from "../keybindings"
import {
  type EditorAdapter,
  type ExternalUrlAdapter,
  type ImagePasteAdapter,
  type NotificationAdapter,
  type TextClipboardAdapter,
} from "../platform"
import { type PresentationFrameScheduler } from "../presentation"
import { type ClientCommand, type CommandOutcome, type EngineEvent } from "../protocol"
import { type RottweilerState } from "../state"
import { type RottweilerTheme } from "../theme"

export interface RottweilerAppOptions {
  readonly diagnostics?: ClientDiagnostics | undefined
  readonly sessionReader: SessionReader
  readonly initialEvent?: EngineEvent
  readonly initialState?: RottweilerState
  readonly sessionId?: string
  readonly clientId?: string
  readonly onCommand?: (
    command: ClientCommand,
  ) => void | CommandOutcome | null | Promise<void | CommandOutcome | null>
  readonly onProviderApiKey?: (
    provider: string,
    apiKey: string,
    allocation: ReplyAllocation,
  ) => Promise<{
    readonly stored: true
    readonly activated: boolean
    readonly warnings: readonly string[]
  }>
  readonly onProviderActivate?: (provider: string) => Promise<void>
  readonly requestId?: () => string
  /** Presentation clock. Production uses wall time; deterministic fixtures may inject a fixed value. */
  readonly nowMs?: () => number
  readonly theme?: RottweilerTheme
  readonly systemThemeMode?: ThemeMode | null
  readonly systemTheme?: RottweilerTheme
  readonly treeSitterClient?: TreeSitterClient
  readonly notifications?: NotificationAdapter
  readonly editor?: EditorAdapter
  readonly imagePaste?: ImagePasteAdapter
  readonly externalUrl?: ExternalUrlAdapter
  readonly textClipboard?: TextClipboardAdapter
  readonly terminalHandover?: TerminalHandoverAdapter
  readonly onSessionSelect?: (sessionId: string) => void | Promise<void>
  /** Close the complete supervised application. The supervisor reaps its owned engine. */
  readonly onExit?: () => void
  /** Historical presentation is observer-only; the composer and mutating interactions are hidden. */
  readonly replaySessionId?: string
  /** TUI-local bindings. Standard is the canonical map; Vim enables modal editing/navigation. */
  readonly keybindings?: KeybindingConfiguration
  /** Injectable frame scheduler used to coalesce presentation-only stream deltas. */
  readonly presentationFrame?: PresentationFrameScheduler
  /** Startup instrumentation invoked only after the composer accepts changed input. */
  readonly onComposerInput?: (value: string) => void
  /** Host platform used for terminal compatibility decoding. Injectable for production-path tests. */
  readonly platform?: NodeJS.Platform
}

export interface TerminalHandoverAdapter {
  suspend(): void
  resume(): void
}
