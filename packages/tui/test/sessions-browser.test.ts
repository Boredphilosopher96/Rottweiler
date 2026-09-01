import { describe, expect, test } from "bun:test"
import { createSessionsBrowserModel } from "../src/sessions-browser"
import type { RottweilerState } from "../src/state/model"

const sessions: RottweilerState["sessions"] = [{
  sessionId: "session-auth", title: "Auth refactor", workspaceName: "rottweiler", model: "gpt-5", shellActive: true, driverClientId: null,
}]

describe("sessions browser model", () => {
  test("projects truthful list-detail rows and filters the remote catalog", () => {
    const model = createSessionsBrowserModel({ catalog: { kind: "ready", sessions, truncated: false }, query: "auth", selectedId: null })
    expect(model.rows.map((row) => row.id)).toEqual(["sessions.new", "session-auth"])
    expect(model.rows[1]).toMatchObject({ label: "Auth refactor", action: { kind: "manage" }, detail: { description: expect.stringContaining("shell       active") } })
    expect(JSON.stringify(model)).not.toMatch(/transcript preview|tokens|cost/i)
  })

  test("distinguishes loading, errors, stale data, and truncated results", () => {
    expect(createSessionsBrowserModel({ catalog: { kind: "loading" }, query: "", selectedId: null })).toMatchObject({ rows: [], emptyCopy: "Loading sessions" })
    const failed = createSessionsBrowserModel({ catalog: { kind: "error", message: "offline", stale: sessions }, query: "", selectedId: "session-auth" })
    expect(failed).toMatchObject({ selectedId: "session-auth", notice: { message: "offline", tone: "error" } })
    expect(failed.rows.map((row) => row.id)).toEqual(["sessions.new", "sessions.retry", "session-auth"])
    expect(createSessionsBrowserModel({ catalog: { kind: "ready", sessions, truncated: true }, query: "", selectedId: null }).title).toContain("truncated")
  })
})
