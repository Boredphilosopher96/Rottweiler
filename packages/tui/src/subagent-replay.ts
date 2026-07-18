export const MAX_BUFFERED_SUBAGENT_LIVE_EVENTS = 4_096
export const MAX_BUFFERED_SUBAGENT_LIVE_BYTES = 8 * 1_024 * 1_024

export interface SubagentReplayEvent<T> {
  readonly sequence: string
  readonly event: T
  readonly bytes: number
}

interface SubagentReplayBase<T> {
  readonly childSessionId: string
  readonly verifiedCursor: string | null
  readonly declaredTail: string | null | undefined
  readonly historyTruncatedAt: string | null
  readonly bufferedEvents: readonly SubagentReplayEvent<T>[]
  readonly bufferedBytes: number
  readonly bufferedCount: number
  readonly overflowed: boolean
  readonly gapDetected: boolean
  readonly omittedPrefixStart: string | null
}

export type SubagentReplayState<T> =
  | SubagentReplayBase<T> & {
      readonly status: "idle"
    }
  | SubagentReplayBase<T> & {
      readonly status: "replaying"
      readonly requestId: string | null
      readonly afterSequence: string | null
    }
  | SubagentReplayBase<T> & {
      readonly status: "failed"
      readonly retryFrom: string | null
      readonly failure: "rejected" | "transport_lost" | "unavailable" | "exception"
    }
  | SubagentReplayBase<T> & {
      readonly status: "caughtUp"
    }

export type SubagentReplayInput<T> =
  | { readonly type: "enter"; readonly childSessionId: string }
  | {
      readonly type: "requestIssued"
      readonly requestId: string
      readonly afterSequence: string | null
    }
  | {
      readonly type: "replayBatch"
      readonly requestId: string
      readonly childSessionId: string
      readonly events: readonly {
        readonly sequence: string
        readonly eventSequence: string | null
        readonly event: T
      }[]
    }
  | {
      readonly type: "replayCompleted"
      readonly requestId: string
      readonly childSessionId: string
      readonly throughSequence: string | null
      readonly nextCursor: string | null
      readonly tailSequence: string | null
      readonly hasMore: boolean
      readonly eventsBeforePage: string
      readonly truncated: boolean
    }
  | {
      readonly type: "liveProgress"
      readonly childSessionId: string
      readonly childSequence: string | null
      readonly eventSequence: string | null
      readonly event: T
      readonly bytes: number
    }
  | {
      readonly type: "rejected"
      readonly requestId: string
      readonly failure: "rejected" | "unavailable" | "exception"
    }
  | { readonly type: "transportLost" }
  | { readonly type: "reconnected" }
  | { readonly type: "historyTruncated"; readonly eventsBeforePage: string }
  | { readonly type: "overflow" }
  | { readonly type: "close" }

export type SubagentReplayEffect<T> =
  | { readonly type: "requestPage"; readonly afterSequence: string | null }
  | { readonly type: "applyEvents"; readonly events: readonly T[] }
  | {
      readonly type: "bufferProgress"
      readonly bufferedCount: number
      readonly overflowed: boolean
    }
  | { readonly type: "drainBuffer"; readonly events: readonly T[] }
  | { readonly type: "noticeRestart"; readonly reason: string }
  | { readonly type: "resetProjection" }
  | {
      readonly type: "replayFailed"
      readonly failure: "rejected" | "unavailable" | "exception"
    }
  | { readonly type: "none" }

export interface SubagentReplayTransition<T> {
  readonly state: SubagentReplayState<T>
  readonly effects: readonly SubagentReplayEffect<T>[]
}

export function createSubagentReplayState<T>(
  childSessionId: string,
  verifiedCursor: string | null = null,
): SubagentReplayState<T> {
  return {
    status: "idle",
    childSessionId,
    verifiedCursor: normalizedSequence(verifiedCursor),
    declaredTail: undefined,
    historyTruncatedAt: null,
    bufferedEvents: [],
    bufferedBytes: 0,
    bufferedCount: 0,
    overflowed: false,
    gapDetected: false,
    omittedPrefixStart: null,
  }
}

export function transitionSubagentReplay<T>(
  state: SubagentReplayState<T>,
  input: SubagentReplayInput<T>,
): SubagentReplayTransition<T> {
  switch (input.type) {
    case "enter":
      return enter(state, input.childSessionId)
    case "requestIssued":
      return requestIssued(state, input.requestId, input.afterSequence)
    case "replayBatch":
      return replayBatch(state, input)
    case "replayCompleted":
      return replayCompleted(state, input)
    case "liveProgress":
      return liveProgress(state, input)
    case "rejected":
      return rejected(state, input.requestId, input.failure)
    case "transportLost":
      return transportLost(state)
    case "reconnected":
      return reconnected(state)
    case "historyTruncated":
      return historyTruncated(state, input.eventsBeforePage)
    case "overflow":
      return result({ ...state, overflowed: true })
    case "close":
      return result(createSubagentReplayState<T>(state.childSessionId))
  }
}

function enter<T>(
  state: SubagentReplayState<T>,
  childSessionId: string,
): SubagentReplayTransition<T> {
  if (state.childSessionId !== childSessionId) {
    const reset = createSubagentReplayState<T>(childSessionId)
    return result(replaying(reset, null), [
      { type: "resetProjection" },
      { type: "requestPage", afterSequence: null },
    ])
  }
  if (state.status === "failed") return retryFailed(state)
  if (state.historyTruncatedAt !== null) {
    const reset = {
      ...createSubagentReplayState<T>(state.childSessionId),
      status: "replaying" as const,
      requestId: null,
      afterSequence: null,
    }
    return result(reset, [
      { type: "resetProjection" },
      { type: "requestPage", afterSequence: null },
    ])
  }
  if (state.status === "replaying") {
    if (state.verifiedCursor !== null) return result(state)
    const next = { ...state, requestId: null, afterSequence: null }
    return result(next, [{ type: "requestPage", afterSequence: null }])
  }
  if (state.verifiedCursor !== null) {
    return result(caughtUp(state))
  }
  return result(replaying(state, null), [{ type: "requestPage", afterSequence: null }])
}

function requestIssued<T>(
  state: SubagentReplayState<T>,
  requestId: string,
  afterSequence: string | null,
): SubagentReplayTransition<T> {
  if (state.status !== "replaying" || state.afterSequence !== afterSequence) return result(state)
  return result({ ...state, requestId })
}

function replayBatch<T>(
  state: SubagentReplayState<T>,
  input: Extract<SubagentReplayInput<T>, { type: "replayBatch" }>,
): SubagentReplayTransition<T> {
  if (!matchesReplay(state, input.requestId, input.childSessionId) || state.gapDetected) {
    return result(state)
  }
  let verified = parseSequence(state.verifiedCursor)
  let omittedPrefixStart = state.omittedPrefixStart
  let gapDetected = false
  const applied: T[] = []
  for (const item of input.events) {
    const sequence = parseSequence(item.sequence)
    if (
      sequence === null ||
      item.eventSequence !== item.sequence ||
      parseSequence(item.eventSequence) === null
    ) continue
    if (verified !== null && sequence <= verified) continue
    if (verified === null && state.afterSequence === null) {
      if (sequence > 0n) omittedPrefixStart = sequence.toString()
      verified = sequence
      applied.push(item.event)
      continue
    }
    if (sequence !== (verified ?? 0n) + 1n) {
      gapDetected = true
      break
    }
    verified = sequence
    applied.push(item.event)
  }
  const next = {
    ...state,
    verifiedCursor: sequenceString(verified),
    omittedPrefixStart,
    gapDetected,
  }
  return result(next, applied.length === 0 ? [] : [{ type: "applyEvents", events: applied }])
}

function replayCompleted<T>(
  state: SubagentReplayState<T>,
  input: Extract<SubagentReplayInput<T>, { type: "replayCompleted" }>,
): SubagentReplayTransition<T> {
  if (!matchesReplay(state, input.requestId, input.childSessionId)) return result(state)
  const eventsBeforePage = parseSequence(input.eventsBeforePage)
  const omittedPrefixStart = parseSequence(state.omittedPrefixStart)
  if (omittedPrefixStart !== null) {
    if (
      state.afterSequence !== null ||
      !input.truncated ||
      eventsBeforePage === null ||
      eventsBeforePage !== omittedPrefixStart
    ) {
      return restart(
        state,
        "The child transcript omitted an unverified durable prefix; restarting the initial replay.",
        true,
      )
    }
  }
  const pageState = { ...state, omittedPrefixStart: null }
  if (pageState.overflowed) {
    return restart(
      clearBuffer(pageState),
      "Child transcript updates exceeded the safe live buffer; reloading from the durable cursor.",
    )
  }
  if (pageState.gapDetected) {
    return restart(
      pageState,
      "A child transcript page skipped durable events; reloading from the last verified cursor.",
    )
  }
  const through = parseSequence(input.throughSequence)
  const applied = parseSequence(pageState.verifiedCursor)
  if (through !== null && applied !== through) {
    return restart(
      pageState,
      "The child transcript page did not reach its declared cursor; reloading from the last verified event.",
    )
  }
  const tail = parseSequence(input.tailSequence)
  const observedTail = sequenceString(tail)
  if (pageState.declaredTail !== undefined && pageState.declaredTail !== observedTail) {
    return restart(
      pageState,
      "The durable child transcript changed while pages were loading; restarting from the verified cursor.",
    )
  }
  let checkedState: SubagentReplayState<T> = {
    ...pageState,
    declaredTail: pageState.declaredTail === undefined ? observedTail : pageState.declaredTail,
    historyTruncatedAt:
      pageState.afterSequence === null && input.truncated && eventsBeforePage !== null && eventsBeforePage > 0n
        ? eventsBeforePage.toString()
        : pageState.historyTruncatedAt,
  }
  if (input.hasMore) {
    const nextCursor = parseSequence(input.nextCursor)
    const currentCursor = parseSequence(pageState.afterSequence) ?? 0n
    if (
      nextCursor === null ||
      nextCursor <= currentCursor ||
      applied === null ||
      nextCursor !== applied ||
      tail === null ||
      nextCursor >= tail
    ) {
      return restart(
        checkedState,
        "The child transcript returned an invalid next-page cursor; reloading from the last verified event.",
      )
    }
    checkedState = replaying(checkedState, nextCursor.toString())
    return result(checkedState, [{ type: "requestPage", afterSequence: nextCursor.toString() }])
  }
  if (applied !== tail) {
    return restart(
      checkedState,
      "The child transcript stopped before its durable tail; reloading from the last verified event.",
    )
  }
  return drainVerifiedBuffer({ ...checkedState, declaredTail: undefined })
}

function liveProgress<T>(
  state: SubagentReplayState<T>,
  input: Extract<SubagentReplayInput<T>, { type: "liveProgress" }>,
): SubagentReplayTransition<T> {
  if (state.childSessionId !== input.childSessionId) return result(state)
  const eventSequence = parseSequence(input.eventSequence)
  const childSequence = parseSequence(input.childSequence)
  if (
    eventSequence === null ||
    (input.childSequence !== null && childSequence !== eventSequence)
  ) return result(state)
  const entry: SubagentReplayEvent<T> = {
    sequence: eventSequence.toString(),
    event: input.event,
    bytes: input.bytes,
  }
  if (state.status === "replaying") return bufferProgress(state, entry)
  if (state.status === "failed") {
    const buffered = appendBuffer(state, entry)
    const next = replaying({
      ...buffered,
      omittedPrefixStart: null,
      gapDetected: false,
    }, state.retryFrom)
    return result(next, [
      {
        type: "bufferProgress",
        bufferedCount: next.bufferedCount,
        overflowed: next.overflowed,
      },
      { type: "requestPage", afterSequence: state.retryFrom },
    ])
  }
  const verified = parseSequence(state.verifiedCursor)
  if (verified !== null && eventSequence <= verified) return result(state)
  if (verified === null) {
    return result(caughtUp(state, {
      verifiedCursor: eventSequence.toString(),
      historyTruncatedAt: eventSequence > 0n ? eventSequence.toString() : state.historyTruncatedAt,
    }), [{ type: "applyEvents", events: [input.event] }])
  }
  if (eventSequence === verified + 1n) {
    return result(caughtUp(state, {
      verifiedCursor: eventSequence.toString(),
    }), [{ type: "applyEvents", events: [input.event] }])
  }
  const buffered = appendBuffer(state, entry)
  const next = replaying(buffered, verified.toString())
  return result(next, [
    {
      type: "bufferProgress",
      bufferedCount: next.bufferedCount,
      overflowed: next.overflowed,
    },
    { type: "requestPage", afterSequence: verified.toString() },
  ])
}

function rejected<T>(
  state: SubagentReplayState<T>,
  requestId: string,
  failure: "rejected" | "unavailable" | "exception",
): SubagentReplayTransition<T> {
  if (state.status !== "replaying" || state.requestId !== requestId) return result(state)
  const next = failed(state, failure)
  return result(next, [{ type: "replayFailed", failure }])
}

function transportLost<T>(state: SubagentReplayState<T>): SubagentReplayTransition<T> {
  if (state.status !== "replaying") return result(state)
  return result(failed(state, "transport_lost"))
}

function reconnected<T>(state: SubagentReplayState<T>): SubagentReplayTransition<T> {
  if (state.status === "failed") return retryFailed(state)
  if (state.status !== "replaying") return result(state)
  const next = replaying({
    ...state,
    omittedPrefixStart: null,
    gapDetected: false,
  }, state.verifiedCursor)
  return result(next, [{ type: "requestPage", afterSequence: state.verifiedCursor }])
}

function historyTruncated<T>(
  state: SubagentReplayState<T>,
  eventsBeforePage: string,
): SubagentReplayTransition<T> {
  const parsed = parseSequence(eventsBeforePage)
  if (parsed === null || parsed === 0n) return result(state)
  return result({ ...state, historyTruncatedAt: parsed.toString() })
}

function retryFailed<T>(state: Extract<SubagentReplayState<T>, { status: "failed" }>) {
  const next = replaying({
    ...state,
    omittedPrefixStart: null,
    gapDetected: false,
  }, state.retryFrom)
  return result(next, [
    {
      type: "noticeRestart",
      reason: "Retrying the child transcript from the last verified event after the previous replay failed.",
    },
    { type: "requestPage", afterSequence: state.retryFrom },
  ])
}

function restart<T>(
  state: SubagentReplayState<T>,
  reason: string,
  resetProjection = false,
): SubagentReplayTransition<T> {
  const afterSequence = resetProjection ? null : state.verifiedCursor
  const base = resetProjection
    ? {
        ...state,
        verifiedCursor: null,
        historyTruncatedAt: null,
        declaredTail: undefined,
        omittedPrefixStart: null,
        gapDetected: false,
        overflowed: false,
      }
    : {
        ...state,
        declaredTail: undefined,
        omittedPrefixStart: null,
        gapDetected: false,
        overflowed: false,
      }
  return result(replaying(base, afterSequence), [
    ...(resetProjection ? [{ type: "resetProjection" as const }] : []),
    { type: "noticeRestart", reason },
    { type: "requestPage", afterSequence },
  ])
}

function drainVerifiedBuffer<T>(state: SubagentReplayState<T>): SubagentReplayTransition<T> {
  if (state.bufferedEvents.length === 0) {
    return result(caughtUp(clearBuffer(state)))
  }
  const sorted = [...state.bufferedEvents].sort((left, right) =>
    compareSequence(left.sequence, right.sequence),
  )
  let verified = parseSequence(state.verifiedCursor)
  let historyTruncatedAt = state.historyTruncatedAt
  const drained: T[] = []
  let gapIndex = -1
  for (let index = 0; index < sorted.length; index += 1) {
    const item = sorted[index]!
    const sequence = parseSequence(item.sequence)!
    if (verified !== null && sequence <= verified) continue
    if (verified === null) {
      verified = sequence
      if (sequence > 0n) historyTruncatedAt = sequence.toString()
      drained.push(item.event)
      continue
    }
    if (sequence !== verified + 1n) {
      gapIndex = index
      break
    }
    verified = sequence
    drained.push(item.event)
  }
  const drainEffect: SubagentReplayEffect<T>[] = drained.length === 0
    ? []
    : [{ type: "drainBuffer", events: drained }]
  if (gapIndex >= 0) {
    const remaining = sorted.slice(gapIndex)
    const bufferedBytes = remaining.reduce((total, item) => total + item.bytes, 0)
    const next = replaying({
      ...state,
      verifiedCursor: sequenceString(verified),
      historyTruncatedAt,
      declaredTail: undefined,
      bufferedEvents: remaining,
      bufferedBytes,
      bufferedCount: remaining.length,
      overflowed: false,
      gapDetected: false,
      omittedPrefixStart: null,
    }, sequenceString(verified))
    return result(next, [
      ...drainEffect,
      { type: "requestPage", afterSequence: sequenceString(verified) },
    ])
  }
  return result(caughtUp(clearBuffer(state), {
    verifiedCursor: sequenceString(verified),
    historyTruncatedAt,
    declaredTail: undefined,
  }), drainEffect)
}

function bufferProgress<T>(
  state: SubagentReplayState<T>,
  event: SubagentReplayEvent<T>,
): SubagentReplayTransition<T> {
  const next = appendBuffer(state, event)
  return result(next, [{
    type: "bufferProgress",
    bufferedCount: next.bufferedCount,
    overflowed: next.overflowed,
  }])
}

function appendBuffer<T>(
  state: SubagentReplayState<T>,
  event: SubagentReplayEvent<T>,
): SubagentReplayState<T> {
  if (state.overflowed) return state
  if (
    state.bufferedCount >= MAX_BUFFERED_SUBAGENT_LIVE_EVENTS ||
    event.bytes > MAX_BUFFERED_SUBAGENT_LIVE_BYTES ||
    state.bufferedBytes + event.bytes > MAX_BUFFERED_SUBAGENT_LIVE_BYTES
  ) return { ...state, overflowed: true }
  const bufferedEvents = [...state.bufferedEvents, event]
  return {
    ...state,
    bufferedEvents,
    bufferedBytes: state.bufferedBytes + event.bytes,
    bufferedCount: bufferedEvents.length,
  }
}

function replaying<T>(
  state: SubagentReplayState<T>,
  afterSequence: string | null,
): Extract<SubagentReplayState<T>, { status: "replaying" }> {
  return { ...replayBase(state), status: "replaying", requestId: null, afterSequence }
}

function failed<T>(
  state: Extract<SubagentReplayState<T>, { status: "replaying" }>,
  failure: Extract<SubagentReplayState<T>, { status: "failed" }>["failure"],
): Extract<SubagentReplayState<T>, { status: "failed" }> {
  return { ...replayBase(state), status: "failed", retryFrom: state.verifiedCursor, failure }
}

function caughtUp<T>(
  state: SubagentReplayState<T>,
  overrides: Partial<SubagentReplayBase<T>> = {},
): Extract<SubagentReplayState<T>, { status: "caughtUp" }> {
  return { ...replayBase(state), ...overrides, status: "caughtUp" }
}

function replayBase<T>(state: SubagentReplayState<T>): SubagentReplayBase<T> {
  return {
    childSessionId: state.childSessionId,
    verifiedCursor: state.verifiedCursor,
    declaredTail: state.declaredTail,
    historyTruncatedAt: state.historyTruncatedAt,
    bufferedEvents: state.bufferedEvents,
    bufferedBytes: state.bufferedBytes,
    bufferedCount: state.bufferedCount,
    overflowed: state.overflowed,
    gapDetected: state.gapDetected,
    omittedPrefixStart: state.omittedPrefixStart,
  }
}

function clearBuffer<T>(state: SubagentReplayState<T>): SubagentReplayState<T> {
  return {
    ...state,
    bufferedEvents: [],
    bufferedBytes: 0,
    bufferedCount: 0,
    overflowed: false,
  }
}

function matchesReplay<T>(
  state: SubagentReplayState<T>,
  requestId: string,
  childSessionId: string,
): state is Extract<SubagentReplayState<T>, { status: "replaying" }> {
  return state.status === "replaying" &&
    state.requestId === requestId &&
    state.childSessionId === childSessionId
}

function result<T>(
  state: SubagentReplayState<T>,
  effects: readonly SubagentReplayEffect<T>[] = [],
): SubagentReplayTransition<T> {
  return { state, effects: effects.length === 0 ? [{ type: "none" }] : effects }
}

function normalizedSequence(value: string | null): string | null {
  return sequenceString(parseSequence(value))
}

function parseSequence(value: string | null): bigint | null {
  if (value === null || !/^(0|[1-9][0-9]*)$/.test(value)) return null
  try {
    return BigInt(value)
  } catch {
    return null
  }
}

function sequenceString(value: bigint | null): string | null {
  return value?.toString() ?? null
}

function compareSequence(left: string, right: string): number {
  const leftSequence = parseSequence(left)!
  const rightSequence = parseSequence(right)!
  return leftSequence < rightSequence ? -1 : leftSequence > rightSequence ? 1 : 0
}
