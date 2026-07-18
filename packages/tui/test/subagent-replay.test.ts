import { describe, expect, test } from "bun:test"

import {
  MAX_BUFFERED_SUBAGENT_LIVE_BYTES,
  createSubagentReplayState,
  transitionSubagentReplay,
  type SubagentReplayEffect,
  type SubagentReplayInput,
  type SubagentReplayState,
} from "../src/subagent-replay"

type State = SubagentReplayState<string>
type Input = SubagentReplayInput<string>

const childSessionId = "child-session"

function advance(state: State, input: Input) {
  return transitionSubagentReplay(state, input)
}

function begin(requestId = "request-1") {
  let state = createSubagentReplayState<string>(childSessionId)
  state = advance(state, { type: "enter", childSessionId }).state
  state = advance(state, { type: "requestIssued", requestId, afterSequence: null }).state
  return state
}

function batch(
  state: State,
  requestId: string,
  events: readonly (readonly [sequence: string, event: string])[],
) {
  return advance(state, {
    type: "replayBatch",
    requestId,
    childSessionId,
    events: events.map(([sequence, event]) => ({ sequence, eventSequence: sequence, event })),
  })
}

function completed(
  state: State,
  requestId: string,
  overrides: Partial<Extract<Input, { type: "replayCompleted" }>> = {},
) {
  return advance(state, {
    type: "replayCompleted",
    requestId,
    childSessionId,
    throughSequence: null,
    nextCursor: null,
    tailSequence: null,
    hasMore: false,
    eventsBeforePage: "0",
    truncated: false,
    ...overrides,
  })
}

function effectsOfType<T extends SubagentReplayEffect<string>["type"]>(
  effects: readonly SubagentReplayEffect<string>[],
  type: T,
) {
  return effects.filter((effect): effect is Extract<SubagentReplayEffect<string>, { type: T }> =>
    effect.type === type,
  )
}

describe("subagent replay state machine", () => {
  test.each([
    ["nonadvancing", "1", null, [], "1"],
    ["not applied", "2", null, [], "1"],
    ["at the tail", "3", "3", [["2", "page two"], ["3", "page three"]], "3"],
  ] as const)(
    "rejects a %s next-page cursor",
    (_label, nextCursor, throughSequence, pageEvents, expectedAfter) => {
      let state = begin()
      state = batch(state, "request-1", [["1", "page one"]]).state
      state = completed(state, "request-1", {
        throughSequence: "1",
        nextCursor: "1",
        tailSequence: "3",
        hasMore: true,
        eventsBeforePage: "1",
        truncated: true,
      }).state
      state = advance(state, {
        type: "requestIssued",
        requestId: "request-2",
        afterSequence: "1",
      }).state
      state = batch(state, "request-2", pageEvents).state

      const transition = completed(state, "request-2", {
        throughSequence,
        nextCursor,
        tailSequence: "3",
        hasMore: true,
        eventsBeforePage: "1",
        truncated: true,
      })

      expect(transition.state).toMatchObject({ status: "replaying", afterSequence: expectedAfter })
      expect(effectsOfType(transition.effects, "noticeRestart")[0]?.reason).toContain(
        "invalid next-page cursor",
      )
      expect(effectsOfType(transition.effects, "requestPage")).toEqual([
        { type: "requestPage", afterSequence: expectedAfter },
      ])
    },
  )

  test("ignores stale batches, completions, and rejections after correlation replacement", () => {
    let state = begin("stale-request")
    state = advance(state, { type: "transportLost" }).state
    state = advance(state, { type: "reconnected" }).state
    state = advance(state, {
      type: "requestIssued",
      requestId: "current-request",
      afterSequence: null,
    }).state

    const staleInputs: readonly Input[] = [
      {
        type: "replayBatch",
        requestId: "stale-request",
        childSessionId,
        events: [{ sequence: "1", eventSequence: "1", event: "stale batch" }],
      },
      {
        type: "replayCompleted",
        requestId: "stale-request",
        childSessionId,
        throughSequence: "1",
        nextCursor: null,
        tailSequence: "1",
        hasMore: false,
        eventsBeforePage: "1",
        truncated: true,
      },
      { type: "rejected", requestId: "stale-request", failure: "rejected" },
    ]
    for (const input of staleInputs) {
      const transition = advance(state, input)
      expect(transition.state).toEqual(state)
      expect(transition.effects).toEqual([{ type: "none" }])
    }
  })

  test("restarts from the verified cursor when a declared tail changes between pages", () => {
    let state = begin()
    state = batch(state, "request-1", [["1", "page one"]]).state
    state = completed(state, "request-1", {
      throughSequence: "1",
      nextCursor: "1",
      tailSequence: "3",
      hasMore: true,
      eventsBeforePage: "1",
      truncated: true,
    }).state
    state = advance(state, {
      type: "requestIssued",
      requestId: "request-2",
      afterSequence: "1",
    }).state
    state = batch(state, "request-2", [["2", "page two"]]).state

    const transition = completed(state, "request-2", {
      throughSequence: "2",
      nextCursor: "2",
      tailSequence: "4",
      hasMore: true,
      eventsBeforePage: "1",
      truncated: true,
    })

    expect(transition.state).toMatchObject({
      status: "replaying",
      afterSequence: "2",
      declaredTail: undefined,
    })
    expect(effectsOfType(transition.effects, "noticeRestart")[0]?.reason).toContain(
      "changed while pages were loading",
    )
  })

  test.each([
    ["accepted", true, "9", false],
    ["mismatched start", true, "8", true],
    ["not declared truncated", false, "9", true],
  ] as const)("validates a truncated first page: %s", (_label, truncated, eventsBeforePage, restarts) => {
    let state = begin()
    state = batch(state, "request-1", [["9", "retained start"], ["10", "retained tail"]]).state
    const transition = completed(state, "request-1", {
      throughSequence: "10",
      tailSequence: "10",
      eventsBeforePage,
      truncated,
    })

    if (restarts) {
      expect(transition.state).toMatchObject({ status: "replaying", afterSequence: null })
      expect(effectsOfType(transition.effects, "resetProjection")).toHaveLength(1)
    } else {
      expect(transition.state).toMatchObject({
        status: "caughtUp",
        verifiedCursor: "10",
        historyTruncatedAt: "9",
      })
      expect(effectsOfType(transition.effects, "noticeRestart")).toHaveLength(0)
    }
  })

  test("marks overflow, discards the unsafe buffer, and replays from the durable cursor", () => {
    let state = begin()
    const overflow = advance(state, {
      type: "liveProgress",
      childSessionId,
      childSequence: "1",
      eventSequence: "1",
      event: "oversized",
      bytes: MAX_BUFFERED_SUBAGENT_LIVE_BYTES + 1,
    })
    expect(overflow.state).toMatchObject({
      status: "replaying",
      overflowed: true,
      bufferedCount: 0,
    })
    state = overflow.state

    const transition = completed(state, "request-1")
    expect(transition.state).toMatchObject({
      status: "replaying",
      afterSequence: null,
      overflowed: false,
      bufferedCount: 0,
    })
    expect(effectsOfType(transition.effects, "noticeRestart")[0]?.reason).toContain(
      "exceeded the safe live buffer",
    )
  })

  test("drains buffered progress exactly once after the declared tail is verified", () => {
    let state = begin()
    state = advance(state, {
      type: "liveProgress",
      childSessionId,
      childSequence: "2",
      eventSequence: "2",
      event: "buffered live event",
      bytes: 32,
    }).state
    state = batch(state, "request-1", [["1", "durable event"]]).state
    const firstCompletion = completed(state, "request-1", {
      throughSequence: "1",
      tailSequence: "1",
      eventsBeforePage: "1",
      truncated: true,
    })

    expect(firstCompletion.state).toMatchObject({
      status: "caughtUp",
      verifiedCursor: "2",
      bufferedCount: 0,
    })
    expect(effectsOfType(firstCompletion.effects, "drainBuffer")).toEqual([
      { type: "drainBuffer", events: ["buffered live event"] },
    ])

    const duplicateCompletion = completed(firstCompletion.state, "request-1", {
      throughSequence: "1",
      tailSequence: "1",
      eventsBeforePage: "1",
      truncated: true,
    })
    expect(duplicateCompletion.effects).toEqual([{ type: "none" }])
    expect(effectsOfType(duplicateCompletion.effects, "drainBuffer")).toHaveLength(0)
  })

  test("retries a failed second page from its verified cursor when the child is re-entered", () => {
    let state = begin()
    state = batch(state, "request-1", [["1", "page one"]]).state
    state = completed(state, "request-1", {
      throughSequence: "1",
      nextCursor: "1",
      tailSequence: "2",
      hasMore: true,
      eventsBeforePage: "1",
      truncated: true,
    }).state
    state = advance(state, {
      type: "requestIssued",
      requestId: "request-2",
      afterSequence: "1",
    }).state
    state = advance(state, {
      type: "liveProgress",
      childSessionId,
      childSequence: "3",
      eventSequence: "3",
      event: "buffered after the declared tail",
      bytes: 32,
    }).state

    const failure = advance(state, {
      type: "rejected",
      requestId: "request-2",
      failure: "rejected",
    })
    expect(failure.state).toMatchObject({
      status: "failed",
      retryFrom: "1",
      declaredTail: "2",
      bufferedCount: 1,
    })

    const retry = advance(failure.state, { type: "enter", childSessionId })
    expect(retry.state).toMatchObject({
      status: "replaying",
      afterSequence: "1",
      declaredTail: "2",
    })
    expect(effectsOfType(retry.effects, "noticeRestart")[0]?.reason).toContain("Retrying")
    expect(effectsOfType(retry.effects, "requestPage")).toEqual([
      { type: "requestPage", afterSequence: "1" },
    ])

    state = advance(retry.state, {
      type: "requestIssued",
      requestId: "request-3",
      afterSequence: "1",
    }).state
    state = batch(state, "request-3", [["2", "page two"]]).state
    const recovered = completed(state, "request-3", {
      throughSequence: "2",
      tailSequence: "2",
      eventsBeforePage: "1",
      truncated: true,
    })
    expect(recovered.state).toMatchObject({
      status: "caughtUp",
      verifiedCursor: "3",
      bufferedCount: 0,
    })
    expect(effectsOfType(recovered.effects, "drainBuffer")).toEqual([{
      type: "drainBuffer",
      events: ["buffered after the declared tail"],
    }])
  })

  test("retries from the retained cursor and tail after reconnect", () => {
    let state = begin()
    state = batch(state, "request-1", [["1", "page one"]]).state
    state = completed(state, "request-1", {
      throughSequence: "1",
      nextCursor: "1",
      tailSequence: "2",
      hasMore: true,
      eventsBeforePage: "1",
      truncated: true,
    }).state
    state = advance(state, {
      type: "requestIssued",
      requestId: "request-2",
      afterSequence: "1",
    }).state

    const lost = advance(state, { type: "transportLost" })
    expect(lost.state).toMatchObject({
      status: "failed",
      retryFrom: "1",
      declaredTail: "2",
      failure: "transport_lost",
    })
    const recovered = advance(lost.state, { type: "reconnected" })
    expect(recovered.state).toMatchObject({
      status: "replaying",
      afterSequence: "1",
      declaredTail: "2",
    })
    expect(effectsOfType(recovered.effects, "requestPage")).toEqual([
      { type: "requestPage", afterSequence: "1" },
    ])
  })
})
