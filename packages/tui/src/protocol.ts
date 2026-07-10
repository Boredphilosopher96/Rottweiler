import type {
  ClientCommand as GeneratedClientCommand,
  EngineEvent as GeneratedEngineEvent,
} from "../../../protocol/types"

/**
 * The TUI only consumes protocol types generated from the Rust source of truth.
 * Keeping this boundary in one module makes that ownership explicit and gives
 * the future transport client a stable local import.
 */
export type ClientCommand = GeneratedClientCommand
export type EngineEvent = GeneratedEngineEvent
