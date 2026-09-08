import { expect, test } from "bun:test"
import validateRequest from "../src/generated/provider-request-validator.js"
import validateEvent from "../src/generated/provider-event-validator.js"
import type { ProviderRequest, Block } from "../src/generated/provider-contract"

const request: ProviderRequest = {
  model: "fixture", turns: [{ role: "user", blocks: [{ type: "text", text: "hello" }], meta: { created_at: null, model: null, synthetic: false, summary: false } }],
  tools: [], tool_choice: { mode: "auto" }, max_output_tokens: 64,
  temperature: null, thinking: "off", cache_hint: null,
}

test("provider requests require complete owned fields and typed conversation blocks", () => {
  expect(validateRequest(request)).toBe(true)
  for (const field of Object.keys(request)) {
    const incomplete: Record<string, unknown> = { ...request }
    delete incomplete[field]
    expect(validateRequest(incomplete)).toBe(false)
  }
  for (const block of [{ type: "audio", data: "opaque" }, { type: "text", text: "hello", role: "system" }, { type: "tool_result", id: "call", output: { type: "unknown" }, is_error: false }]) {
    expect(validateRequest({ ...request, turns: [{ role: "user", blocks: [block], meta: request.turns[0]?.meta }] })).toBe(false)
  }
  expect(validateRequest({ ...request, extra: true })).toBe(false)
})

test("provider events reject malformed variants before delivery", () => {
  expect(validateEvent({ type: "text_delta", text: "hello" })).toBe(true)
  expect(validateEvent({ type: "text_delta", text: "hello", arguments: {} })).toBe(false)
  expect(validateEvent({ type: "thinking_delta", content: "reason" })).toBe(false)
  expect(validateEvent({ type: "thinking_delta", content: "reason", signature: null })).toBe(true)
  expect(validateEvent({ type: "unknown" })).toBe(false)
  expect(validateEvent({ type: "route_selected", route: "forged" })).toBe(false)
})

test("conversation metadata and content require explicit nullable fields", () => {
  const turn: ProviderRequest["turns"][number] = {
    role: "assistant",
    meta: { created_at: null, model: null, synthetic: false, summary: false },
    blocks: [
      { type: "thinking", content: "reason", signature: null },
      { type: "citation", uri: "https://example.com", title: null, excerpt: null },
    ],
  }
  expect(validateRequest({ ...request, turns: [turn] })).toBe(true)
  for (const field of Object.keys(turn.meta)) {
    const meta: Record<string, unknown> = { ...turn.meta }
    delete meta[field]
    expect(validateRequest({ ...request, turns: [{ ...turn, meta }] })).toBe(false)
  }
  for (const block of turn.blocks) {
    for (const field of Object.keys(block)) {
      const incomplete: Record<string, unknown> = { ...block }
      delete incomplete[field]
      expect(validateRequest({ ...request, turns: [{ ...turn, blocks: [incomplete] }] })).toBe(false)
    }
  }
})

// @ts-expect-error Provider content is a closed semantic union.
const unsupported: Block = { type: "audio", data: "opaque" }
void unsupported
