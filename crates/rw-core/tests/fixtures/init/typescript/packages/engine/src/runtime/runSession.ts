import { randomUUID } from 'node:crypto'

/**
 * A "session" in Sora's sense = one continuous browser run, shared across every
 * window. Splitting tabs into a second window is still the same session. This id
 * is generated once per process start and is the primitive History Tree and
 * other session-scoped features build on.
 */
export const RUN_SESSION_ID = randomUUID()
export const RUN_STARTED_AT = Date.now()
