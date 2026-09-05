import { describe, expect, test } from "bun:test"
import { contractFixture } from "../../../protocol/fixtures/contract"
import eventSchema from "../../../protocol/schema/engine-event.schema.json"
import { PROTOCOL_VERSION } from "../src/protocol"
import { isWireEngineEvent, normalizeWireEngineEvent } from "../src/transport"

describe("generated engine event validation", () => {
  test("accepts every Rust-authored fixture without coercion or projection changes", () => {
    for (const event of contractFixture.engine_events) {
      const before = structuredClone(event)
      expect(normalizeWireEngineEvent(event)).toBe(event)
      expect(event).toEqual(before)
    }
  })

  test("rejects the reproduced known discriminator without its required payload", () => {
    expect(normalizeWireEngineEvent({ type: "command_acknowledged" })).toBeNull()
    expect(isWireEngineEvent({ type: "command_acknowledged" })).toBeFalse()
  })

  test("covers every known discriminator in the generated owner schema", () => {
    for (const variant of eventSchema.oneOf) {
      expect(normalizeWireEngineEvent({ type: variant.properties.type.const })).toBeNull()
    }
  })

  test("requires the fields declared by the owner schema for every fixture variant", () => {
    for (const event of contractFixture.engine_events) {
      const variant = eventSchema.oneOf.find((variant) => variant.properties.type.const === event.type)
      if (variant === undefined) throw new Error(`missing event schema: ${event.type}`)
      for (const field of variant.required.filter((field) => field !== "type")) {
        const malformed: Record<string, unknown> = { ...event }
        delete malformed[field]
        expect(normalizeWireEngineEvent(malformed), `${event.type}.${field}`).toBeNull()
      }
    }
  })

  test("rejects malformed nested payloads and unsupported protocol versions", () => {
    const event = contractFixture.engine_events.find((event) => event.type === "text_delta")
    if (event === undefined) throw new Error("missing text fixture")
    for (const malformed of [
      { ...event, text: [] },
      { ...event, meta: null },
      { ...event, meta: { ...event.meta, emitted_at: 123 } },
      { ...event, meta: { ...event.meta, protocol_version: PROTOCOL_VERSION + 1 } },
    ]) expect(normalizeWireEngineEvent(malformed)).toBeNull()
  })

  test("preserves exact u64 strings and rejects invalid durable cursors", () => {
    const event = contractFixture.engine_events.find((event) => event.type === "text_delta")
    if (event === undefined) throw new Error("missing text fixture")
    for (const sequence of ["0", "9007199254740993", "18446744073709551615"]) {
      const valid = { ...event, meta: { ...event.meta, sequence_id: sequence } }
      expect(normalizeWireEngineEvent(valid)).toBe(valid)
    }
    for (const sequence of ["", "01", "-1", "18446744073709551616", 1, "bad"]) {
      expect(normalizeWireEngineEvent({ ...event, meta: { ...event.meta, sequence_id: sequence } })).toBeNull()
    }
  })

  test("rejects unsupported discriminators and undeclared object fields", () => {
    const unknown = { type: "future_event", meta: { sequence_id: "42" }, payload: [1] }
    expect(normalizeWireEngineEvent(unknown)).toBeNull()
    const event = contractFixture.engine_events[0]
    if (event === undefined) throw new Error("empty event fixture")
    const known = { ...event, additive_field: true }
    expect(normalizeWireEngineEvent(known)).toBeNull()
    for (const invalid of [null, [], {}, { type: 1 }]) expect(normalizeWireEngineEvent(invalid)).toBeNull()
  })

  test("rejects undeclared metadata on transient and connection events", () => {
    const transient = contractFixture.engine_events.find((event) => event.type === "compaction_attempt_started")
    const connection = contractFixture.engine_events.find((event) => event.type === "command_acknowledged")
    if (transient === undefined || connection === undefined) throw new Error("missing event fixtures")
    for (const meta of [null, { sequence_id: "99" }]) {
      const event = { ...transient, meta }
      expect(normalizeWireEngineEvent(event)).toBeNull()
    }
    const event = { ...connection, meta: { ...connection.meta, sequence_id: "99" } }
    expect(normalizeWireEngineEvent(event)).toBeNull()
  })
})
