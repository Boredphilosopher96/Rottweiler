import { describe, expect, test } from "bun:test"

import { presentError, sanitizeErrorFragment } from "../src/render"

describe("presentError", () => {
  test("uses stable copy for known engine and TUI codes", () => {
    expect(presentError({
      category: "protocol",
      code: "session_requires_recovery",
      message: "session is fail-closed until checkpoint journal recovery completes",
    })).toEqual({
      text: "Restoring this session · input will be available shortly",
      severity: "warning",
    })
    expect(presentError({
      category: "protocol",
      code: "provider_activation_pending",
      message: "credential stored, activation pending",
    })).toEqual({
      text: "Credential stored securely · activation is pending",
      severity: "info",
    })
  })

  test("classifies transient and terminal failures by severity", () => {
    expect(presentError({
      category: "protocol",
      code: "subagents_unavailable",
    }).severity).toBe("warning")
    expect(presentError({
      category: "provider",
      code: "permission_denied",
      message: "credential was rejected",
    }).severity).toBe("error")
    expect(presentError({
      category: "protocol",
      code: "activation_pending",
      message: "activation pending",
    }).severity).toBe("info")
  })

  test("sanitizes unknown error fragments before presenting them", () => {
    const result = presentError({
      category: "extension",
      code: "opaque_failure",
      message: "socket\u0007 refused\n    at connect (/private/tmp/client.ts:42:7)",
    })
    expect(result).toEqual({
      text: "Something went wrong · socket refused",
      severity: "error",
    })
    expect(result.text).not.toContain("client.ts")
    expect(result.text).not.toContain("\u0007")
  })

  test("bounds unknown fragments and appends a safe request identifier", () => {
    const result = presentError({
      category: "protocol",
      code: "opaque_failure",
      message: "x".repeat(240),
      requestId: "request-42",
    })
    expect(result.text).toBe(`Something went wrong · ${"x".repeat(159)}… · request request-42`)
    expect(result.severity).toBe("error")
  })

  test("sanitizes fragments without replacing TUI-authored framing", () => {
    expect(sanitizeErrorFragment("socket\u0007 refused\n    at connect (/private/tmp/client.ts:42:7)")).toBe(
      "socket refused",
    )
    expect(sanitizeErrorFragment("x".repeat(240))).toBe(`${"x".repeat(159)}…`)
  })
})
