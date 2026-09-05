import { type ListDetailRow } from "../../src/components"
import {
  PROTOCOL_VERSION,
  type PermissionModeDescriptor,
  type PermissionStateDescriptor
} from "../../src/protocol"
import type { ActivityPresentation, ToolsWorkspacePresentation } from "../../src/render"

export function meta(sequence: string) {
  return {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-components",
    sequence_id: sequence,
    emitted_at: "2026-01-01T00:00:00Z",
  }
}

export async function waitFor(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const deadline = performance.now() + timeoutMs
  while (!predicate()) {
    if (performance.now() >= deadline) throw new Error("timed out waiting for component state")
    await Bun.sleep(5)
  }
}

export function permissionState(runtimeMode: PermissionModeDescriptor): PermissionStateDescriptor {
  return {
    default: "ask" as const,
    effective_rules: [],
    project_rules: [],
    session_rules: [],
    approvals: [],
    truncated: false,
    runtime_mode: runtimeMode,
  }
}

export function rgba(hex: string): [number, number, number, number] {
  return [
    Number.parseInt(hex.slice(1, 3), 16),
    Number.parseInt(hex.slice(3, 5), 16),
    Number.parseInt(hex.slice(5, 7), 16),
    255,
  ]
}

export const listDetailRows: readonly ListDetailRow<string>[] = [
  { kind: "section", id: "section.conversation", label: "Conversation" },
  {
    kind: "item",
    id: "compact",
    label: "Compact context",
    matchSpans: [],
    detail: { title: "Compact context", description: "Compact the conversation context", meta: "Conversation · built-in" },
    action: "compact",
  },
  {
    kind: "item",
    id: "rewind",
    label: "Rewind to a turn",
    matchSpans: [[0, 2], [7, 9]],
    detail: { title: "Rewind to a turn", description: "Choose from completed user turns", meta: "Conversation · built-in" },
    action: "rewind",
  },
  ...Array.from({ length: 22 }, (_, index): ListDetailRow<string> => ({
    kind: "item",
    id: `command-${index}`,
    label: `/command-${index}`,
    matchSpans: [],
    detail: { title: `/command-${index}`, description: `Run command ${index}`, meta: "Commands · extension" },
    action: `command-${index}`,
  })),
]

export function toolsActivity(
  toolCallId: string,
  visibleLines: number,
  outcome: "running" | "succeeded",
  hiddenRetainedLineCount: number,
): Extract<ActivityPresentation, { readonly kind: "tool" }> {
  return {
    kind: "tool",
    key: `tool:${toolCallId}`,
    toolCallId,
    name: "bash",
    subject: `bun test ${toolCallId}`,
    outcome: outcome === "running"
      ? { kind: "running", label: "live" }
      : { kind: "succeeded", label: "Completed" },
    elapsed: { kind: "known", milliseconds: 12_000, label: "00:12" },
    output: {
      kind: "text",
      text: Array.from({ length: visibleLines }, (_, index) => `${toolCallId}-${index + 1}`).join("\n"),
      retainedLineCount: visibleLines + hiddenRetainedLineCount,
      visibleLineCount: visibleLines,
      hiddenRetainedLineCount,
      window: outcome === "running" ? "tail" : "head",
      sourceTruncated: false,
    },
    defaultExpanded: outcome === "running",
    canOpenRetainedOutput: true,
  }
}

export function toolsWorkspaceModel(rows: readonly ActivityPresentation[]): ToolsWorkspacePresentation {
  return {
    replay: false,
    rows,
    turn: {
      kind: "running",
      turnId: "turn-tools",
      toolCount: rows.filter((row) => row.kind === "tool").length,
      liveCount: rows.filter((row) => row.kind === "tool" && row.outcome.kind === "running").length,
      deniedCount: 0,
      elapsed: { kind: "known", milliseconds: 12_000, label: "00:12" },
      usage: null,
      cost: null,
    },
    queuedMessages: [],
  }
}

export function neverUsage() {
  return {
    input_tokens: "0",
    output_tokens: "0",
    cache_read_tokens: "0",
    cache_write_tokens: "0",
    reasoning_tokens: "0",
  }
}
