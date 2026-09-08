import { writeFile } from "node:fs/promises"
import { join } from "node:path"
import { createRottweilerApp, type RottweilerApp } from "../app"
import { ClientAllocationOwner } from "../client-allocation"
import { createInitialState } from "../state"
import { readTuiRecycleState, writeTuiRecycleState } from "../recycle-state"
import { MemoryFixture } from "./memory-fixture"
import { createMemoryRenderer } from "./memory-renderer"

function requireThat(value: unknown, message: string): asserts value { if (!value) throw new Error(message) }

/** Actual HTTP and native renderer; each invocation owns one application generation. */
export async function runReviewRecycleProbe(directory: string, mode: "capture" | "restore" | "changed" | "removed", view: "session" | "workspace" = "session"): Promise<void> {
  const allocations = new ClientAllocationOwner()
  const fixture = new MemoryFixture(join(directory, `review-${process.pid}.sock`), allocations,
    mode === "changed" || mode === "removed" ? mode : "multiple")
  const { setup, treeSitter } = await createMemoryRenderer()
  let app: RottweilerApp | null = null
  let decisions = 0
  const initial = createInitialState()
  const handoffPath = join(directory, "review-handoff.json")
  let observed: unknown = null
  const until = async (condition: () => boolean) => {
    const deadline = performance.now() + 10_000
    while (!condition()) {
      if (performance.now() >= deadline) throw new Error(`Review generation did not settle: ${JSON.stringify({ mode, errors: app?.state.errors, review: app?.recycleState()?.review })}`)
      await Bun.sleep(1); await setup.renderOnce(); app?.applyPendingRecycleScroll()
    }
    await setup.renderOnce(); app?.applyPendingRecycleScroll()
  }
  try {
    app = createRottweilerApp(setup.renderer, { allocations, treeSitterClient: treeSitter, sessionId: "memory-probe", clientId: "memory-client", sessionReader: fixture.reader,
      initialState: { ...initial, connection: { ...initial.connection, phase: "connected" }, driverClientId: "memory-client",
        workspaceStatus: { workspaceName: "Review", branch: "main", changedPaths: ["held-review2.txt"], truncated: false },
        workspaceRoots: { generation: "1", effectiveFromTurn: "0", roots: ["/review-workspace"] } },
      async onCommand(command, allocation) {
        if (command.type === "review_file") decisions++
        const reply = await fixture.command(command, allocation)
        if (reply.type === "read") for (const event of reply.events) app?.handleEvent(event)
        const completion = fixture.connectionCompletion(command)
        if (completion !== null) app?.handleEvent(completion)
        return reply.outcome
      },
    })
    setup.renderer.root.add(app)
    await until(() => app!.transcript.mountedCards.size > 0)
    if (mode === "capture") {
      app.composer.restoreDraft("retained review draft 界", [{ name: "notes.txt", media_type: "text/plain", data: { type: "text", content: "accepted attachment" } }])
      if (view === "session") {
        app.openReview()
        await until(() => app!.state.review?.files.length === 3)
        requireThat(app.reviewPanel.selectPath("held-review2.txt"), "exact selected review file missing")
      } else {
        app.contextPanel.changedFiles.selectCurrent()
        await until(() => app!.state.workspaceDiff !== null)
      }
      await setup.renderOnce(); await setup.renderOnce()
      app.reviewPanel.diffScroller.scrollTo({ x: 0, y: 40 })
      await setup.renderOnce()
      requireThat(app.reviewPanel.diff.height <= app.reviewPanel.diffScroller.viewport.height, "native diff viewport grew with retained source lines")
      requireThat(setup.captureCharFrame().includes("old content 20"), `native diff did not display the selected source line:\n${setup.captureCharFrame()}`)
      const handoff = app.recycleState()
      requireThat(handoff?.review?.path === "held-review2.txt" && handoff.review.scrollTop === 40, "native review viewport was not captured")
      requireThat(!JSON.stringify(handoff.review).includes("old content"), "review handoff retained its source body")
      requireThat(writeTuiRecycleState(handoffPath, handoff), "review handoff was not durably written")
      observed = handoff.review
      process.exitCode = 75
    } else {
      using allocation = allocations.reserve("decoding", 0)
      const decoded = readTuiRecycleState(handoffPath, allocation)
      requireThat(decoded !== null && decoded.state.review !== null, "review handoff could not be decoded")
      const expected = decoded.state.review
      fixture.hold()
      requireThat(app.restoreRecycleState(decoded.state), "review editing state was not adopted")
      decoded.consume(); allocation.release()
      await until(() => fixture.pending > 0)
      requireThat(app.recycleState() === null, "review recycled before fresh source validation")
      setup.mockInput.pressKey("a"); setup.mockInput.pressKey("r")
      requireThat(decisions === 0, "review acted before fresh source validation")
      fixture.release()
      if (mode === "restore") {
        await until(() => app!.recycleState()?.review !== undefined && app!.recycleState()?.review !== null)
        requireThat(setup.captureCharFrame().includes("old content 20"), "restored native diff did not display the selected source line")
        const restored = app.recycleState()?.review
        requireThat(JSON.stringify(restored) === JSON.stringify(expected), "review source or native viewport changed across generations")
        requireThat(app.reviewPanel.visible && (view === "session" ? app.reviewPanel.files.focused : app.reviewPanel.diffScroller.focused), "review focus was not restored")
        observed = restored
        if (view === "session") {
        fixture.hold()
        setup.mockInput.pressKey("a")
        await until(() => decisions === 1 && fixture.pending > 0)
        requireThat(app.recycleState() === null, "pending review decision allowed recycle")
        setup.mockInput.pressEscape()
        requireThat(app.recycleState() === null, "closing review released the unsettled decision owner")
        fixture.release()
        await until(() => app!.recycleState() !== null)
        } else {
          setup.mockInput.pressKey("a"); setup.mockInput.pressKey("r")
          requireThat(decisions === 0, "worktree diff acquired session decision authority")
          setup.mockInput.pressEscape()
        }
      } else {
        await until(() => app!.state.errors.some(error => error.code === "review_restore_source_changed"))
        requireThat(app.reviewPanel.visible && app.reviewPanel.details.plainText.includes("changed or disappeared"), "stale source refusal was not visible")
        setup.mockInput.pressKey("a"); setup.mockInput.pressKey("r")
        requireThat(decisions === 0 && app.recycleState() === null, "stale review regained mutation or recycle authority")
        observed = { refused: true, mode }
        setup.mockInput.pressEscape()
      }
      requireThat(app.composer.value === "retained review draft 界", "review restoration lost the accepted draft")
      requireThat(app.composer.attachments[0]?.data.type === "text" && app.composer.attachments[0].data.content === "accepted attachment", "review restoration lost its attachment")
    }
  } finally {
    fixture.release(); app?.destroy(); app = null
    await fixture.close()
    try {
      const deadline = performance.now() + 10_000
      while (allocations.usage.bytes !== 0 && performance.now() < deadline) await Bun.sleep(1)
    } finally { setup.renderer.destroy() }
  }
  requireThat(allocations.usage.bytes === 0, `review generation retained allocation: ${JSON.stringify(allocations.usage)}`)
  await writeFile(join(directory, `${mode}.json`), JSON.stringify({ schemaVersion: 1, pid: process.pid, bunVersion: Bun.version, view, mode, observed, decisions, finalAllocationBytes: allocations.usage.bytes }))
}
