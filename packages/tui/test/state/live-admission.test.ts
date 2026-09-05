import { expect, test } from "bun:test"
import { MAX_PENDING_QUESTION_REQUESTS, MAX_QUESTION_SET_BYTES, MAX_TURN_CITATIONS, MAX_CITATION_TEXT_BYTES, MAX_TURN_CITATION_TEXT_BYTES } from "../../../../protocol/types"
import { createInitialState } from "../../src/state"
import type { EngineEvent } from "../../src/protocol"
import { meta, reduce } from "./fixtures"

const asked = (sequence: number, id: string, prompt = "Choose"): EngineEvent => ({
  type: "question_asked", meta: meta(String(sequence)), turn_id: "1", question_id: id,
  questions: [{ id, prompt, response_kind: "text", options: [] }],
})
const answered = (sequence: number, id: string): EngineEvent => ({
  type: "question_answered", meta: meta(String(sequence)), turn_id: "1", question_id: id,
  answers: [{ question_id: id, values: ["answer"] }],
})
const citation = (sequence: number, uri = "https://example.test"): EngineEvent => ({
  type: "citation_delta", meta: meta(String(sequence)), turn_id: "1", uri, title: null,
})

test("thousands of settled questions leave no completed payloads while pending questions remain", () => {
  let state = reduce(createInitialState(), asked(1, "pending"))
  for (let index = 0; index < 1000; index++) {
    state = reduce(state, asked(index * 2 + 2, String(index)))
    state = reduce(state, answered(index * 2 + 3, String(index)))
    expect(Object.keys(state.questions)).toEqual(["pending"])
  }
  expect(state.questions.pending?.questions[0]?.prompt).toBe("Choose")
})

test("question count and escaped byte limits reject without advancing the durable cursor", () => {
  let state = createInitialState()
  for (let index = 0; index < MAX_PENDING_QUESTION_REQUESTS; index++) state = reduce(state, asked(index, String(index)))
  const sequence = state.lastSequence
  expect(() => reduce(state, asked(MAX_PENDING_QUESTION_REQUESTS, "overflow"))).toThrow("admission")
  expect(state.lastSequence).toBe(sequence)
  expect(Object.keys(state.questions)).toHaveLength(MAX_PENDING_QUESTION_REQUESTS)
  state = reduce(state, answered(MAX_PENDING_QUESTION_REQUESTS, "0"))
  expect(Object.keys(reduce(state, asked(MAX_PENDING_QUESTION_REQUESTS + 1, "fits")).questions)).toHaveLength(MAX_PENDING_QUESTION_REQUESTS)
  expect(() => reduce(createInitialState(), asked(0, "large", "\0".repeat(MAX_QUESTION_SET_BYTES / 2)))).toThrow("admission")
})

test("citations count and bytes remain bounded across the complete agent turn", () => {
  let state = createInitialState()
  for (let index = 0; index < MAX_TURN_CITATIONS; index++) state = reduce(state, citation(index))
  expect(() => reduce(state, citation(MAX_TURN_CITATIONS))).toThrow("admission")
  expect(state.streamingTail?.citations).toHaveLength(MAX_TURN_CITATIONS)
  let bytes = createInitialState()
  const text = "x".repeat(MAX_CITATION_TEXT_BYTES)
  const entries = MAX_TURN_CITATION_TEXT_BYTES / MAX_CITATION_TEXT_BYTES
  for (let index = 0; index < entries; index++) bytes = reduce(bytes, citation(index, text))
  expect(bytes.streamingTail?.citationBytes).toBe(MAX_TURN_CITATION_TEXT_BYTES)
  expect(() => reduce(bytes, citation(entries, "x"))).toThrow("admission")
  const nextTurn = reduce(bytes, { ...citation(entries, "new"), turn_id: "2" } as EngineEvent)
  expect(nextTurn.streamingTail?.citationBytes).toBe(3)
  expect(() => reduce(createInitialState(), { ...citation(0, text), title: "extra" } as EngineEvent)).toThrow("admission")
})
