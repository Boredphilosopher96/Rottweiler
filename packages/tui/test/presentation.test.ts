import { describe, expect, test } from "bun:test"

import { PresentationController } from "../src/presentation"
import { deferPresentationForEvent } from "../src/presentation"

describe("presentation controller", () => {
  test("batches replay updates into one presentation while suspended", () => {
    const presentations: Array<{ pending: readonly number[]; dirty: boolean }> = []
    const completed: number[] = []
    const controller = new PresentationController<number>({
      scheduler: undefined,
      destroyed: () => false,
      present: (pending, dirty) => presentations.push({ pending: [...pending], dirty }),
      afterPresent: (item) => completed.push(item),
    })

    controller.suspend()
    controller.enqueue(1, false)
    controller.enqueue(2, true)
    controller.markDirty(false)
    controller.flushBeforeStateChange()
    controller.flush()

    expect(presentations).toEqual([])
    expect(completed).toEqual([])

    controller.resume()

    expect(presentations).toEqual([{ pending: [2], dirty: true }])
    expect(completed).toEqual([2])
  })

  test("a stalled frame retains the final display revision and preserves the next control effect", () => {
    const presented: number[][] = [], completed: number[] = []
    let scheduled = 0
    const controller = new PresentationController<number>({
      scheduler: { schedule() { scheduled++; return 1 }, cancel() {} }, destroyed: () => false,
      present: pending => presented.push([...pending]), afterPresent: item => completed.push(item),
    })
    for (let revision = 0; revision < 100_000; revision++) controller.enqueue(revision, true)
    expect(scheduled).toBe(1)
    expect(presented).toEqual([])
    controller.enqueue(100_000, false)
    expect(presented).toEqual([[99_999, 100_000]])
    expect(completed).toEqual([99_999, 100_000])
    controller.enqueue(100_001, true)
    controller.flush()
    expect(presented.at(-1)).toEqual([100_001])
    controller.destroy()
  })

  test("only display-only events defer; snapshots and interaction resolutions retain immediate effects", () => {
    for (const type of ["text_delta", "thinking_delta", "tool_output_delta", "tool_progress", "citation_delta", "compaction_text_delta"] as const) {
      expect(deferPresentationForEvent({ type })).toBe(true)
    }
    for (const type of ["session_state_ready", "session_controls_ready", "tool_approval_resolved", "question_asked", "todos_read", "tool_call_finished", "ui_panels_ready"] as const) {
      expect(deferPresentationForEvent({ type })).toBe(false)
    }
  })
})
