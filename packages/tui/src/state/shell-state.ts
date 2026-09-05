import {
  type EngineEvent
} from "../protocol"
import { boundedUtf8 } from "./display-buffer"
import {
  type RottweilerState
} from "./model"

export const MAX_SHELL_COMMAND_BYTES = 8 * 1_024

export const MAX_SHELL_OUTPUT_BYTES = 64 * 1_024

export const MAX_SHELL_OUTPUT_LINES = 32

type UserShellStateChangedEvent = Extract<EngineEvent, { type: "user_shell_state_changed" }>

export function projectShellEvent(
  state: RottweilerState,
  event: UserShellStateChangedEvent,
): RottweilerState {
  const existing = state.latestShell?.shellId === event.shell_id ? state.latestShell : null
  const commandSource = typeof event.command === "string"
    ? event.command
    : existing?.command ?? "Shell command"
  const command = boundedUtf8(sanitizeShellText(commandSource.slice(0, MAX_SHELL_COMMAND_BYTES * 2)).trim(), MAX_SHELL_COMMAND_BYTES)
  const outputSource = event.captured_output ?? existing?.capturedOutput ?? ""
  const sourceClipped = outputSource.length > MAX_SHELL_OUTPUT_BYTES * 2
  const rawOutput = sanitizeShellText(outputSource.slice(0, MAX_SHELL_OUTPUT_BYTES * 2))
  const lineBound = boundedShellLines(rawOutput, MAX_SHELL_OUTPUT_LINES)
  const capturedOutput = boundedUtf8(sourceClipped ? `${lineBound}\n… additional output omitted` : lineBound, MAX_SHELL_OUTPUT_BYTES)
  const outputTruncated = sourceClipped ||
    rawOutput.split("\n").length > MAX_SHELL_OUTPUT_LINES ||
    Buffer.byteLength(lineBound) > MAX_SHELL_OUTPUT_BYTES
  const shell = {
    shellId: event.shell_id,
    command: command === "" ? "Shell command" : command,
    active: event.active,
    status: event.status ?? existing?.status ?? null,
    capturedOutput,
    outputTruncated,
  } as const
  return { ...state, hasActivity: true, latestShell: shell,
    shell: { shellId: event.shell_id, active: event.active, status: shell.status, capturedOutput } }

}

function sanitizeShellText(value: string): string {
  return value
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n")
    // OSC and CSI escape sequences are terminal instructions, not retained
    // transcript content. Removing them prevents output from changing the UI.
    .replace(/\u001b\][^\u0007]*(?:\u0007|\u001b\\)/g, "")
    .replace(/\u001b(?:\[[0-?]*[ -/]*[@-~]|.)/g, "")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001a\u001c-\u001f\u007f]/g, "")
}

function boundedShellLines(value: string, maximum: number): string {
  const lines = value.split("\n")
  if (lines.length <= maximum) return value
  return [
    ...lines.slice(0, Math.max(0, maximum - 1)),
    `… ${lines.length - maximum + 1} more lines`,
  ].join("\n")
}
