import {
  createTestRenderer,
  type TestRenderer
} from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { reviewLineCounts } from "../../src/components"
import {
  PROTOCOL_VERSION,
  type ClientCommand,
  type CommandOutcome
} from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"
import { waitFor } from "./fixtures"

describe("review components", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("renders cumulative review and routes exact per-file accept or revert commands", async () => {
    const setup = await createTestRenderer({ width: 112, height: 32, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const review = {
      sessionId: "session-review",
      files: [
        {
          path: "src/lib.rs",
          unifiedDiff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
          status: "pending" as const,
          truncated: true,
          unrestorableReason: null,
          originalHash: "old",
          currentHash: "new",
        },
        {
          path: "generated.bin",
          unifiedDiff: "Binary files differ",
          status: "pending" as const,
          truncated: false,
          unrestorableReason: "original bytes were not checkpointed",
          originalHash: "absent",
          currentHash: "generated",
        },
      ],
    }
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: { ...createInitialState(), review },
      sessionId: "session-review",
      clientId: "review-client",
      requestId: () => "review-request",
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
    app.openReview()
    await setup.renderOnce()

    expect(app.reviewPanel.visible).toBeTrue()
    expect(app.reviewPanel.summary.plainText).toContain("2 pending")
    expect(app.reviewPanel.diff.diff).toContain("+new")
    setup.mockInput.pressKey("r")
    expect(commands).toContainEqual({
      type: "review_file",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "review-client",
        request_id: "review-request",
      },
      session_id: "session-review",
      path: "src/lib.rs",
      decision: "revert",
      current_hash: "new",
    })

    commands.length = 0
    app.reviewPanel.files.setSelectedIndex(1)
    setup.mockInput.pressKey("r")
    expect(commands).toEqual([])
    expect(app.reviewPanel.hint.plainText).toContain("revert unavailable")
    setup.mockInput.pressKey("a")
    expect(commands).toContainEqual(expect.objectContaining({
      type: "review_file",
      path: "generated.bin",
      decision: "accept",
      current_hash: "generated",
    }))
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.reviewPanel.visible).toBeFalse()
    expect(app.composer.visible).toBeTrue()
    expect(app.state.review).toEqual(review)
  })

  test("uses the full-primary review layout and collapses its detail rail on narrow terminals", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        review: {
          sessionId: "session-review-layout",
          files: [
            {
              path: "src/cursor.rs",
              unifiedDiff: "--- a/src/cursor.rs\n+++ b/src/cursor.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+added\n context\n",
              status: "pending",
              truncated: false,
              unrestorableReason: null,
              originalHash: "old-cursor",
              currentHash: "new-cursor",
            },
            {
              path: "docs/protocol.md",
              unifiedDiff: "--- a/docs/protocol.md\n+++ b/docs/protocol.md\n@@ -1 +1 @@\n-before\n+after\n",
              status: "accepted",
              truncated: false,
              unrestorableReason: null,
              originalHash: "old-docs",
              currentHash: "new-docs",
            },
          ],
        },
      },
    })
    renderer.root.add(app)
    app.openReview()
    await setup.renderOnce()

    expect(app.reviewPanel.x).toBe(0)
    expect(app.reviewPanel.y).toBe(0)
    expect(app.reviewPanel.width).toBe(110)
    expect(app.reviewPanel.height).toBe(app.composer.y)
    expect(app.composer.visible).toBeTrue()
    expect(app.reviewPanel.leftPane.width).toBe(73)
    expect(app.reviewPanel.rightRail.x).toBe(73)
    expect(app.reviewPanel.rightRail.width).toBe(37)
    expect(app.reviewPanel.details.x).toBe(75)
    expect(app.reviewPanel.summary.plainText).toContain("SESSION REVIEW")
    expect(app.reviewPanel.summary.plainText).toContain("2 files")
    expect(app.reviewPanel.summary.plainText).toContain("+3")
    expect(app.reviewPanel.summary.plainText).toContain("−2")
    expect(app.reviewPanel.details.plainText).toContain("THIS FILE")
    expect(app.reviewPanel.details.plainText).toContain("lines     +2 −1")
    expect(app.reviewPanel.details.plainText).toContain("DECISIONS")
    expect(app.reviewPanel.details.plainText).toContain("1 accepted")
    expect(setup.captureCharFrame()).not.toContain("╭─ Session review")

    setup.resize(109, 18)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.reviewPanel.width).toBe(109)
    expect(app.reviewPanel.leftPane.width).toBe(109)
    expect(app.reviewPanel.rightRail.visible).toBeFalse()

    setup.resize(110, 18)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.reviewPanel.leftPane.width).toBe(73)
    expect(app.reviewPanel.rightRail.visible).toBeTrue()

    setup.resize(72, 18)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.reviewPanel.width).toBe(72)
    expect(app.reviewPanel.leftPane.width).toBe(72)
    expect(app.reviewPanel.rightRail.visible).toBeFalse()
    expect(app.reviewPanel.diff.visible).toBeTrue()
    expect(app.reviewPanel.hint.plainText).toContain("accept")
    expect(app.reviewPanel.hint.plainText).toContain("revert")
  })

  test("counts changed content whose text starts like a diff header", () => {
    expect(reviewLineCounts(
      "--- a/file\n+++ b/file\n@@ -1 +1 @@\n--- removed-leading-dashes\n+++ added-leading-pluses\n",
    )).toEqual({ additions: 1, deletions: 1 })
    expect(reviewLineCounts(
      "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n--- still removed\n+++ still added\n",
    )).toEqual({ additions: 2, deletions: 2 })
  })

  test("keeps one review decision in flight and surfaces a stale fingerprint rejection", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    let resolveDecision!: (outcome: CommandOutcome) => void
    const decision = new Promise<CommandOutcome>((resolve) => {
      resolveDecision = resolve
    })
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        review: {
          sessionId: "session-stale-review",
          files: [
            {
              path: "src/stale.rs",
              unifiedDiff: "--- a/src/stale.rs\n+++ b/src/stale.rs\n@@ -1 +1 @@\n-old\n+new\n",
              status: "pending",
              truncated: false,
              unrestorableReason: null,
              originalHash: "original-state",
              currentHash: "displayed-state",
            },
          ],
        },
      },
      sessionId: "session-stale-review",
      onCommand(command) {
        commands.push(command)
        return command.type === "review_file" ? decision : { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openReview()
    await setup.renderOnce()

    setup.mockInput.pressKey("r")
    setup.mockInput.pressKey("r")
    expect(commands.filter((command) => command.type === "review_file")).toHaveLength(1)
    expect(app.reviewPanel.hint.plainText).toContain("Decision pending")

    resolveDecision({
      type: "rejected",
      error: {
        category: "protocol",
        code: "stale_review_fingerprint",
        message: "the file changed since this review was displayed",
        retryable: true,
      },
    })
    await waitFor(() => app.state.errors.at(-1)?.code === "stale_review_fingerprint")
    expect(app.state.errors.at(-1)?.code).toBe("stale_review_fingerprint")
    expect(app.banner.plainText).toContain("file changed since this review")
    expect(app.reviewPanel.hint.plainText).not.toContain("pending")
  })
})
