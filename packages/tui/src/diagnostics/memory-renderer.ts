import { Writable } from "node:stream"
import { addDefaultParsers, getTreeSitterClient } from "@opentui/core"
import { createTestRenderer } from "@opentui/core/testing"
import { embeddedParserConfigurations, materializeTreeSitterRuntime } from "../tree-sitter-runtime"
import { stabilizeTreeSitterClient } from "../tree-sitter-client"

/** A synchronous terminal sink: only counters survive each native write. */
class MemoryProbeTerminal extends Writable {
  readonly isTTY = true
  readonly columns = 110
  readonly rows = 36
  bytes = 0
  writes = 0
  largestWriteBytes = 0
  constructor() { super({ highWaterMark: 64 * 1024 }) }
  getColorDepth() { return 24 }
  override _write(chunk: Buffer, _encoding: BufferEncoding, done: (error?: Error | null) => void) {
    this.bytes += chunk.byteLength
    this.writes++
    this.largestWriteBytes = Math.max(this.largestWriteBytes, chunk.byteLength)
    done()
  }
  get snapshot() {
    return { bytes: this.bytes, writes: this.writes, largestWriteBytes: this.largestWriteBytes,
      queuedBytes: this.writableLength }
  }
}

export async function createMemoryRenderer() {
  const runtime = await materializeTreeSitterRuntime()
  process.env.OTUI_ASSET_ROOT = runtime.root
  process.env.OTUI_TREE_SITTER_WORKER_PATH = runtime.workerPath
  addDefaultParsers(embeddedParserConfigurations(runtime.assetsPath))
  const treeSitter = stabilizeTreeSitterClient(getTreeSitterClient())
  await treeSitter.initialize()
  // Stream terminal bytes into a draining sink with observable byte counters. The test renderer's
  // memory destination retains every ANSI write until destruction and cannot
  // represent a long-lived terminal's resident memory.
  const terminal = new MemoryProbeTerminal()
  // OpenTUI accepts Node writable streams here; the TTY facade supplies its size
  // and color queries without attaching a real terminal to a diagnostic process.
  const setup = await createTestRenderer({ width: 110, height: 36, useThread: false,
    stdout: terminal as unknown as NodeJS.WriteStream, bufferedOutput: "stdout", remote: false })
  return { setup, treeSitter, terminal }

}
