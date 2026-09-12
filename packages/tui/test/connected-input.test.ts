import { expect, spyOn, test } from "bun:test"
import { appendFileSync, renameSync } from "node:fs"
import { mkdtemp, rm, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import {
  joinedInteractiveInput,
  nativeRichInput,
  type JoinedInteractiveInput,
  type NativeRichInput,
} from "../src/diagnostics/connected-input"

function nativeFixture(): NativeRichInput {
  return {
    socketPath: "/private/native.sock",
    bootstrapTokenFile: "/private/native.token",
    sessionId: "rich-native",
    impairmentSocketPath: "/private/control.sock",
  }
}

function joinedFixture(): JoinedInteractiveInput {
  return {
    socketPath: "/private/joined.sock",
    bootstrapTokenFile: "/private/joined.token",
    sessionId: "joined-native",
    history: { conversations: 5000, conversation_items: 10_000, text_bytes: 10_240_000,
      seed_timed: false, provider_context_reset: true, first_source: "1", source_through: "200",
      source_digest: Array.from({ length: 32 }, (_, index) => index) },
    streamLines: 2000,
    streamLine: "joined stream line: bounded native rendering\n",
    host_kind: "optimized-production-EngineHost",
    http_kind: "bounded-test-forwarder",
  }
}

async function temporaryInput(name: string, value: unknown): Promise<{ directory: string; path: string }> {
  const directory = await mkdtemp(join(tmpdir(), "rw-connected-input-"))
  const path = join(directory, name)
  await writeFile(path, JSON.stringify(value), { mode: 0o600 })
  return { directory, path }
}

test("connected diagnostics require their complete exact input contracts", async () => {
  const native = await temporaryInput("native-rich-input.json", nativeFixture())
  const joined = await temporaryInput("joined-input.json", joinedFixture())
  try {
    expect(await nativeRichInput(native.directory)).toEqual(nativeFixture())
    expect(await joinedInteractiveInput(joined.directory)).toEqual(joinedFixture())
    await writeFile(native.path, JSON.stringify({ ...nativeFixture(), unexpected: true }))
    await expect(nativeRichInput(native.directory)).rejects.toThrow("input contract")
    const malformed = { ...joinedFixture(), history: { ...joinedFixture().history, unexpected: true } }
    await writeFile(joined.path, JSON.stringify(malformed))
    await expect(joinedInteractiveInput(joined.directory)).rejects.toThrow("history input contract")
  } finally {
    await rm(native.directory, { recursive: true, force: true })
    await rm(joined.directory, { recursive: true, force: true })
  }
})

test("connected input rejects aliases, oversize bytes, growth and path replacement", async () => {
  const fixture = await temporaryInput("native-rich-input.json", nativeFixture())
  const replacement = join(fixture.directory, "replacement.json")
  const aliasDirectory = await mkdtemp(join(tmpdir(), "rw-connected-alias-"))
  try {
    await writeFile(replacement, JSON.stringify(nativeFixture()))
    const allocation = Buffer.allocUnsafe
    let changed = false
    const replaceDuringRead = spyOn(Buffer, "allocUnsafe").mockImplementation(size => {
      if (!changed) { changed = true; renameSync(replacement, fixture.path) }
      return allocation(size)
    })
    try { await expect(nativeRichInput(fixture.directory)).rejects.toThrow("changed during read") }
    finally { replaceDuringRead.mockRestore() }

    await writeFile(fixture.path, JSON.stringify(nativeFixture()))
    changed = false
    const growDuringRead = spyOn(Buffer, "allocUnsafe").mockImplementation(size => {
      if (!changed) { changed = true; appendFileSync(fixture.path, " ") }
      return allocation(size)
    })
    try { await expect(nativeRichInput(fixture.directory)).rejects.toThrow("changed during read") }
    finally { growDuringRead.mockRestore() }

    await writeFile(fixture.path, "x".repeat(16 * 1024 + 1))
    await expect(nativeRichInput(fixture.directory)).rejects.toThrow("configuration file")
    const alias = join(aliasDirectory, "native-rich-input.json")
    await symlink(fixture.path, alias)
    await expect(nativeRichInput(aliasDirectory)).rejects.toThrow("configuration file")
  } finally {
    await rm(fixture.directory, { recursive: true, force: true })
    await rm(aliasDirectory, { recursive: true, force: true })
  }
})
