import type { UiPresentation, TranscriptContentRead, TranscriptContentPage } from "../../src/protocol"

export function fixturePresentation(): UiPresentation {
  return {
    owner: { extension: "example", generation: "a".repeat(32) },
    descriptor: {
      id: "result", title: "Inspection result", surface: { surface: "tool", tool_name: "read" }, actions: [],
      fields: [
        { kind: "text", id: "summary", label: "Summary" },
        { kind: "badge", id: "status", label: "Status" },
        { kind: "list", id: "checks", label: "Checks", max_items: 32 },
        { kind: "table", id: "files", label: "Files", columns: ["Name", "Result"], max_rows: 32 },
      ],
    },
    projected: {
      truncated: false,
      fields: [
        { kind: "text", id: "summary", value: "Native, source-backed presentation" },
        { kind: "badge", id: "status", value: "Passed" },
        { kind: "list", id: "checks", values: ["Types", "Tests"] },
        { kind: "table", id: "files", rows: [["engine.rs", "Ready"], ["λ.ts", "Ready"]] },
      ],
    },
  }
}

export function surfacePage(value: unknown, read: TranscriptContentRead): TranscriptContentPage {
  const bytes = Buffer.from(JSON.stringify(value))
  let end = Math.min(bytes.length, read.offset + read.max_bytes)
  while (end > read.offset && end < bytes.length && (bytes[end]! & 0xc0) === 0x80) end--
  return {
    view: read.view, source: read.source, offset: read.offset,
    next_offset: end < bytes.length ? end : null,
    total_bytes: bytes.length, format: "json", text: bytes.subarray(read.offset, end).toString("utf8"),
  }
}
