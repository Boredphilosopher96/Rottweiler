import {
  type EngineEvent
} from "../protocol"
import { boundedUtf8 } from "./display-buffer"
import {
  type RottweilerState,
  type TranscriptEntry
} from "./model"
import { retainTranscriptEntry } from "./turn-state"

export const MAX_SHELL_COMMAND_BYTES = 8 * 1_024

export const MAX_SHELL_OUTPUT_BYTES = 64 * 1_024

export const MAX_SHELL_OUTPUT_LINES = 32

type UserShellStateChangedEvent = Extract<EngineEvent, { type: "user_shell_state_changed" }>

export function projectShellEvent(
  state: RottweilerState,
  event: UserShellStateChangedEvent,
  sequenceId: string,
): RottweilerState {
  const agentTurn = `shell:${event.shell_id}`
  const existingIndex = state.transcript.findIndex((entry) => entry.agentTurn === agentTurn)
  const existing = existingIndex < 0 ? undefined : state.transcript[existingIndex]
  const commandSource = typeof event.command === "string"
    ? event.command
    : existing?.shell?.command ?? "Shell command"
  const command = boundedUtf8(sanitizeShellText(commandSource).trim(), MAX_SHELL_COMMAND_BYTES)
  const rawOutput = sanitizeShellText(event.captured_output ?? existing?.shell?.capturedOutput ?? "")
  const lineBound = boundedShellLines(rawOutput, MAX_SHELL_OUTPUT_LINES)
  const capturedOutput = boundedUtf8(lineBound, MAX_SHELL_OUTPUT_BYTES)
  const outputTruncated =
    rawOutput.split("\n").length > MAX_SHELL_OUTPUT_LINES ||
    Buffer.byteLength(lineBound) > MAX_SHELL_OUTPUT_BYTES
  const shell = {
    shellId: event.shell_id,
    command: command === "" ? "Shell command" : command,
    active: event.active,
    status: event.status ?? existing?.shell?.status ?? null,
    capturedOutput,
    outputTruncated,
  } as const
  const entry: TranscriptEntry = {
    sequenceId: existing?.sequenceId ?? sequenceId,
    agentTurn,
    turn: {
      role: "system",
      blocks: [],
      meta: { synthetic: true, summary: false },
    },
    presentation: "shell_result",
    shell,
  }
  const projectedState = {
    ...state,
    shell: { ...state.shell, capturedOutput },
  }
  if (existingIndex < 0) {
    return { ...projectedState, transcript: retainTranscriptEntry(state.transcript, entry) }
  }
  const transcript = [...state.transcript]
  transcript[existingIndex] = entry
  return { ...projectedState, transcript }
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
