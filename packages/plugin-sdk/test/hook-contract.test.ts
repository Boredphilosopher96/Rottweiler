import { expect, test } from "bun:test"
import validateInput from "../src/generated/hook-input-validator.js"
import validateDirective from "../src/generated/hook-directive-validator.js"
import type { HookHandler } from "../src/hooks"

const preTool = { hook: "pre_tool", payload: { id: "call", name: "bash", arguments: { command: "pwd" } } }

test("hook inputs require their complete phase shape and reject foreign fields", () => {
  expect(validateInput(preTool)).toBe(true)
  for (const key of ["id", "name", "arguments"]) {
    const payload: Record<string, unknown> = { ...preTool.payload }
    delete payload[key]
    expect(validateInput({ hook: "pre_tool", payload })).toBe(false)
  }
  expect(validateInput({ ...preTool, extra: true })).toBe(false)
  expect(validateInput({ hook: "pre_tool", payload: { ...preTool.payload, decision: "allow" } })).toBe(false)
  expect(validateInput({ hook: "unknown", payload: preTool.payload })).toBe(false)
  expect(validateInput({ hook: "turn_end", payload: { turn: 1, status: "completed" } })).toBe(false)
  expect(validateInput({ hook: "turn_end", payload: { turn: "1", status: "completed" } })).toBe(true)
})

test("hook transforms expose mutable fields and require complete replacement values", () => {
  expect(validateDirective({ decision: "transform", change: { hook: "pre_tool", name: "read", arguments: {} } })).toBe(true)
  expect(validateDirective({ decision: "transform", change: { hook: "pre_tool", id: "replacement", name: "read", arguments: {} } })).toBe(false)
  expect(validateDirective({ decision: "continue", payload: {} })).toBe(false)
  expect(validateDirective({ decision: "replace", payload: {} })).toBe(false)
  expect(validateDirective({ decision: "permission", value: "ask" })).toBe(true)
  expect(validateDirective({ decision: "permission", value: "continue" })).toBe(false)
})

test("pre_compact requires explicit nullable and continuation policy fields", () => {
  const payload = { reason: "manual", conversation_turns: 2, injected_context: [], replacement_prompt: null, suppress_auto_continue: false }
  expect(validateInput({ hook: "pre_compact", payload })).toBe(true)
  const incomplete: Record<string, unknown> = { ...payload }
  delete incomplete.replacement_prompt
  expect(validateInput({ hook: "pre_compact", payload: incomplete })).toBe(false)
})

// Compile-time phase correlation is checked by the package typecheck.
const transformPrompt: HookHandler<"user_prompt_submit"> = input => ({
  decision: "transform", change: { hook: "user_prompt_submit", content: input.payload.content.toUpperCase() },
})
// @ts-expect-error pre_tool handlers cannot transform post_tool output.
const wrongPhase: HookHandler<"pre_tool"> = () => ({
  decision: "transform",
  change: { hook: "post_tool", output: { type: "text", text: "invalid" }, is_error: false },
})
void transformPrompt
void wrongPhase
