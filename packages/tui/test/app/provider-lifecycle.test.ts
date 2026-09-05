import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp, type RottweilerAppOptions } from "../../src/app"
import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent } from "../../src/protocol"
import { emptyHistoryReader } from "../fixtures/history"

type CredentialResult = Awaited<ReturnType<NonNullable<RottweilerAppOptions["onProviderApiKey"]>>>
const authEvent = (attemptId: string): EngineEvent => ({
  type: "provider_auth_started",
  meta: { protocol_version: PROTOCOL_VERSION, client_id: "tui-client", request_id: `auth-${attemptId}`, emitted_at: "2026-01-01T00:00:00Z" },
  session_id: "session-local",
  attempt_id: attemptId,
  provider: "openai_codex",
  challenge: { kind: "oauth", authorization_url: `https://auth.example.test/${attemptId}`, redirect_uri: "http://127.0.0.1:1455/callback" },
  warnings: [],
})

async function settleContinuations(): Promise<void> {
  await Promise.resolve()
  await Promise.resolve()
}

describe("provider UI lifetime", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  for (const end of ["session switch", "destroy"] as const) {
    for (const outcome of ["success", "failure"] as const) {
      test(`ignores credential ${outcome} after ${end}`, async () => {
        const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
        renderer = setup.renderer
        const pending = Promise.withResolvers<CredentialResult>()
        const started = Promise.withResolvers<void>()
        const commands: ClientCommand[] = []
        const app = createRottweilerApp(renderer, {
          historyReader: emptyHistoryReader,
          onCommand(command) { commands.push(command); return { type: "accepted" } },
          onProviderApiKey() { started.resolve(); return pending.promise },
        })
        renderer.root.add(app)
        app.openProviderApiKeyPrompt("company-openai")
        await setup.mockInput.typeText("secret-lifetime-canary")
        setup.mockInput.pressEnter()
        await started.promise
        if (end === "session switch") app.setSessionId("next-session")
        else app.destroy()
        const count = commands.length
        const state = app.state
        if (outcome === "success") pending.resolve({ stored: true, activated: false, warnings: ["old credential warning"] })
        else pending.reject(new Error("old credential error"))
        await settleContinuations()
        expect(commands).toHaveLength(count)
        expect(app.state).toBe(state)
        expect(JSON.stringify(app.state)).not.toContain("secret-lifetime-canary")
      })
    }
  }

  test("an older browser completion cannot clear or report against a replacement auth attempt", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const first = Promise.withResolvers<void>()
    const second = Promise.withResolvers<void>()
    const urls: string[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand() { return { type: "accepted" } },
      externalUrl: { open(url) { urls.push(url); return urls.length === 1 ? first.promise : second.promise } },
    })
    renderer.root.add(app)
    app.handleEvent(authEvent("first"))
    app.handleEvent(authEvent("second"))
    expect(urls).toHaveLength(2)
    first.reject(new Error("old browser error"))
    await settleContinuations()
    expect(app.state.errors).toHaveLength(0)
    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    expect(urls).toHaveLength(2)
    second.resolve()
    await settleContinuations()
    expect(app.picker.select.options[0]?.description).toContain("Browser opened")
  })
})
