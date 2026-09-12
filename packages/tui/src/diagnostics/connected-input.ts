import { constants } from "node:fs"
import { lstat, open } from "node:fs/promises"
import { isAbsolute, join } from "node:path"

const MAX_CONNECTED_INPUT_BYTES = 16 * 1024

export interface ConnectedInput {
  readonly socketPath: string
  readonly bootstrapTokenFile: string
  readonly sessionId: string
}

export interface NativeRichInput extends ConnectedInput {
  readonly impairmentSocketPath: string
}

export interface JoinedInteractiveInput extends ConnectedInput {
  readonly history: {
    readonly conversations: 5000
    readonly conversation_items: 10_000
    readonly text_bytes: 10_240_000
    readonly seed_timed: false
    readonly provider_context_reset: true
    readonly first_source: string
    readonly source_through: string
    readonly source_digest: readonly number[]
  }
  readonly streamLines: 2000
  readonly streamLine: "joined stream line: bounded native rendering\n"
  readonly host_kind: "optimized-production-EngineHost"
  readonly http_kind: "bounded-test-forwarder"
}

type FileMetadata = Awaited<ReturnType<typeof boundedLstat>>

async function boundedLstat(path: string) {
  return lstat(path, { bigint: true })
}

function sameIdentity(left: FileMetadata, right: FileMetadata): boolean {
  return left.dev === right.dev && left.ino === right.ino && left.mode === right.mode
    && left.nlink === right.nlink && left.size === right.size
    && left.mtimeNs === right.mtimeNs && left.ctimeNs === right.ctimeNs
}

export async function readBoundedPrivateFile(path: string, maximumBytes: number): Promise<Uint8Array> {
  requireThat(Number.isSafeInteger(maximumBytes) && maximumBytes > 0,
    "invalid private file byte bound")
  const named = await boundedLstat(path)
  requireThat(named.isFile() && !named.isSymbolicLink(), "invalid connected probe configuration file")
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK)
  try {
    const admitted = await file.stat({ bigint: true })
    requireThat(admitted.isFile() && admitted.nlink === 1n && admitted.size > 0n
      && admitted.size <= BigInt(maximumBytes) && sameIdentity(named, admitted),
    "invalid connected probe configuration file")
    const size = Number(admitted.size)
    const bytes = Buffer.allocUnsafe(size)
    let offset = 0
    while (offset < size) {
      const { bytesRead } = await file.read(bytes, offset, size - offset, offset)
      requireThat(bytesRead > 0, "connected probe configuration changed during read")
      offset += bytesRead
    }
    const after = await file.stat({ bigint: true })
    const current = await boundedLstat(path)
    requireThat(sameIdentity(admitted, after) && sameIdentity(admitted, current),
      "connected probe configuration changed during read")
    return bytes
  } finally {
    await file.close().catch(() => {})
  }
}

function exactObject(value: unknown, keys: readonly string[], message: string): Record<string, unknown> {
  requireThat(typeof value === "object" && value !== null && !Array.isArray(value), message)
  const record = value as Record<string, unknown>
  const actual = Object.keys(record).sort()
  const expected = [...keys].sort()
  requireThat(actual.length === expected.length && actual.every((key, index) => key === expected[index]), message)
  return record
}

function baseInput(value: Record<string, unknown>): ConnectedInput {
  requireThat(typeof value.socketPath === "string" && isAbsolute(value.socketPath)
    && typeof value.bootstrapTokenFile === "string" && isAbsolute(value.bootstrapTokenFile)
    && typeof value.sessionId === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(value.sessionId),
  "invalid connected probe authority")
  return { socketPath: value.socketPath, bootstrapTokenFile: value.bootstrapTokenFile, sessionId: value.sessionId }
}

async function parsedInput(directory: string, name: string): Promise<unknown> {
  const bytes = await readBoundedPrivateFile(join(directory, name), MAX_CONNECTED_INPUT_BYTES)
  return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes))
}

export async function nativeRichInput(directory: string): Promise<NativeRichInput> {
  const value = exactObject(await parsedInput(directory, "native-rich-input.json"),
    ["socketPath", "bootstrapTokenFile", "sessionId", "impairmentSocketPath"],
    "invalid native rich input contract")
  const common = baseInput(value)
  requireThat(typeof value.impairmentSocketPath === "string" && isAbsolute(value.impairmentSocketPath),
    "missing owned impairment authority")
  return { ...common, impairmentSocketPath: value.impairmentSocketPath }
}

export async function joinedInteractiveInput(directory: string): Promise<JoinedInteractiveInput> {
  const value = exactObject(await parsedInput(directory, "joined-input.json"), [
    "socketPath", "bootstrapTokenFile", "sessionId", "history", "streamLines", "streamLine",
    "host_kind", "http_kind",
  ], "invalid joined interactive input contract")
  const common = baseInput(value)
  const history = exactObject(value.history,
    ["conversations", "conversation_items", "text_bytes", "seed_timed", "provider_context_reset",
      "first_source", "source_through", "source_digest"],
    "invalid joined history input contract")
  requireThat(history.conversations === 5000 && history.conversation_items === 10_000
    && history.text_bytes === 10_240_000 && history.seed_timed === false
    && history.provider_context_reset === true
    && typeof history.first_source === "string" && /^[0-9]+$/.test(history.first_source)
    && typeof history.source_through === "string" && /^[0-9]+$/.test(history.source_through)
    && Array.isArray(history.source_digest) && history.source_digest.length === 32
    && history.source_digest.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255)
    && value.streamLines === 2000 && value.streamLine === "joined stream line: bounded native rendering\n"
    && value.host_kind === "optimized-production-EngineHost" && value.http_kind === "bounded-test-forwarder",
  "joined workload configuration differs")
  return { ...common,
    history: { conversations: history.conversations, conversation_items: history.conversation_items,
      text_bytes: history.text_bytes, seed_timed: history.seed_timed,
      provider_context_reset: history.provider_context_reset, first_source: history.first_source,
      source_through: history.source_through, source_digest: history.source_digest },
    streamLines: value.streamLines, streamLine: value.streamLine,
    host_kind: value.host_kind, http_kind: value.http_kind,
  }
}

function requireThat(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message)
}
