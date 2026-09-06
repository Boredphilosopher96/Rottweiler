import { createTestRenderer } from "@opentui/core/testing"
import { expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type EngineEvent } from "../../src/protocol"
import { emptySessionReader, waitForHistory } from "../fixtures/history"
import { interactionFingerprint, parseInteractionSelection } from "../../src/interaction-selection"

function controls(prompt = "Choose", turn = "2"): EngineEvent {
  return { type: "session_controls_ready", session_id: "s",
    meta: { protocol_version: PROTOCOL_VERSION, client_id: "c", request_id: "r", emitted_at: "2026-01-01T00:00:00Z" },
    snapshot: { through: "25", controls: { approvals: [], pending_plan: null, questions: [{ question_id: "q", turn_id: turn,
      questions: [{ id: "q", prompt, response_kind: "select_one", options: [
        { label: "First", value: "first", description: null }, { label: "Second", value: "second", description: null },
      ] }],
    }] } } }
}

test("interaction highlights survive renders and decoded snapshots, but never changed authority", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  try {
    const app = createRottweilerApp(setup.renderer, { sessionId: "s", sessionReader: emptySessionReader })
    setup.renderer.root.add(app); app.handleEvent(controls())
    await waitForHistory(setup, () => app.interactionPanel.visible)
    app.interactionPanel.select.setSelectedIndex(1)
    const selected = app.interactionPanel.captureSelection()!
    app.setState(app.state); await setup.flush()
    expect(app.interactionPanel.select.getSelectedOption()?.value).toBe("second")
    app.handleEvent(controls()); await setup.flush()
    expect(app.interactionPanel.select.getSelectedOption()?.value).toBe("second")
    app.handleEvent(controls("A different prompt")); await setup.flush()
    expect(app.interactionPanel.select.getSelectedOption()?.value).toBe("first")
    expect(app.interactionPanel.restoreSelection(selected)).toBe(false)
    app.handleEvent(controls("Choose", "3")); await setup.flush()
    expect(app.interactionPanel.restoreSelection(selected)).toBe(false)
    app.handleEvent(controls()); await setup.flush()
    expect(app.interactionPanel.restoreSelection(selected)).toBe(true)
    expect(app.interactionPanel.select.getSelectedOption()?.value).toBe("second")
  } finally { setup.renderer.destroy() }
})

test("local interaction fingerprints frame values and bound selection parsing", () => {
  expect(interactionFingerprint(["ab", "c"])).not.toBe(interactionFingerprint(["a", "bc"]))
  expect(interactionFingerprint(["\ud800"])).not.toBe(interactionFingerprint(["\ufffd"]))
  expect(parseInteractionSelection({ composer: false, fingerprint: "a".repeat(64), index: 2 })).not.toBeNull()
  expect(parseInteractionSelection({ composer: false, fingerprint: "a".repeat(65), index: 2 })).toBeNull()
  expect(parseInteractionSelection({ composer: false, fingerprint: "a".repeat(64), index: -1 })).toBeNull()
})
