import { describe, expect, test } from "bun:test"

import { toolOutputText, TranscriptVirtualizer } from "../src/render"

describe("bounded retained rendering", () => {
  test("summarizes a maximum-size subagent diff before serializing tool output", () => {
    const text = toolOutputText({
      type: "structured",
      value: {
        status: "completed",
        final_text: "done",
        diff_artifact: {
          id: "diff-id",
          base_commit: "0".repeat(40),
          touched_files: Array.from({ length: 4_096 }, (_, index) => ({
            path: `file-${index}.txt`,
            status: "modified",
          })),
          unified_diff: "x".repeat(4 * 1024 * 1024),
        },
      },
    })

    expect(text).toContain("diff-id")
    expect(text).toContain("4194304 chars")
    expect(text).not.toContain("file-4095.txt")
    expect(text.length).toBeLessThan(2_000)
  })

  test("includes bounded child-panel rows in transcript virtual offsets", () => {
    const virtualizer = new TranscriptVirtualizer(0)
    const entries = [
      {
        sequenceId: "1",
        agentTurn: "1",
        turn: {
          role: "assistant" as const,
          blocks: [{ type: "text" as const, text: "first" }],
          meta: { synthetic: false, summary: false },
        },
      },
      {
        sequenceId: "2",
        agentTurn: "2",
        turn: {
          role: "assistant" as const,
          blocks: [{ type: "text" as const, text: "second" }],
          meta: { synthetic: false, summary: false },
        },
      },
    ]
    virtualizer.update(entries, 80, (entry) => (entry.agentTurn === "1" ? 11 : 0))

    expect(virtualizer.heightAt(0) - virtualizer.heightAt(1)).toBe(11)
    expect(virtualizer.window(0, 1).totalHeight).toBe(
      virtualizer.heightAt(0) + virtualizer.heightAt(1),
    )
  })
})
