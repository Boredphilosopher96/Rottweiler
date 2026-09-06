import { expect, test } from "bun:test"
import { createInitialState, MAX_SHELL_OUTPUT_BYTES } from "../src/state"
import { meta, reduce } from "./state/fixtures"

// History bodies belong to the read cache, even when the SSE record is large.
test("committed conversation and command bodies do not become retained live state", () => {
  const text = "source body ".repeat(1024 * 1024)
  let state = createInitialState()
  for (let index = 0; index < 300; index++) {
    state = reduce(state, { type: "conversation_turn_committed", meta: meta(String(index * 2)),
      agent_turn: String(index), turn: { role: "user", blocks: [{ type: "text", text }],
        meta: { synthetic: false, summary: false } } })
    state = reduce(state, { type: "command_finished", meta: meta(String(index * 2 + 1)),
      name: "extension", message: text, unrestorable_paths: [] })
  }
  expect(state.hasActivity).toBe(true)
  expect(state.lastSequence).toBe("599")
  expect("transcript" in state).toBe(false)
  expect(JSON.stringify(state).length).toBeLessThan(10000)
})

test("foreground shell updates retain only the active identity and bounded content", () => {
  let state = createInitialState()
  for (let index = 0; index < 300; index++) {
    state = reduce(state, { type: "user_shell_state_changed", meta: meta(String(index)),
      shell_id: `shell-${index}`, command: "printf hello", active: false, status: 0,
      captured_output: index === 299 ? "\u001b[31mred\u001b[0m\n" + "x".repeat(2 * 1024 * 1024) : `old-${index}` })
  }
  expect(state.latestShell?.shellId).toBe("shell-299")
  expect(state.latestShell?.outputTruncated).toBe(true)
  expect(state.latestShell?.capturedOutput).toStartWith("red\n")
  expect(state.latestShell?.capturedOutput).not.toContain("\u001b")
  expect(Buffer.byteLength(state.latestShell?.capturedOutput ?? "")).toBeLessThanOrEqual(MAX_SHELL_OUTPUT_BYTES)
  expect(state.shell.capturedOutput).toBe(state.latestShell?.capturedOutput ?? null)
  expect(JSON.stringify(state)).not.toContain("old-298")
})
