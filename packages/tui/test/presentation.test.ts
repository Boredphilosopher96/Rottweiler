import { describe, expect, test } from "bun:test"

import { PresentationController } from "../src/presentation"

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

    controller.suspend(true)
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
})
