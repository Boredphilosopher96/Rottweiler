/** Native artifact acceptance: interleaved view and interned-text destruction. */
import { EditBuffer, EditorView, OptimizedBuffer, TextBuffer, TextBufferView, resolveRenderLib, setRenderLibPath } from "@opentui/core"
import { isAbsolute } from "node:path"
import { createHash } from "node:crypto"
import { readFileSync } from "node:fs"

export function probeNativeLifetimes(library: string) {
  if (!isAbsolute(library)) throw new Error("native lifetime probe requires an explicit absolute library")
  setRenderLibPath(library)
  const lib = resolveRenderLib()
  const snapshots = []
  const text = "draft 界e\u0301 ".repeat(8192)
  for (let cycle = 0; cycle < 256; cycle++) {
    const views = Array.from({ length: 16 }, () => {
      const buffer = EditBuffer.create("wcwidth")
      const view = EditorView.create(buffer, 100, 10)
      buffer.setText(text)
      view.getLineInfo()
      if (buffer.getText() !== text) throw new Error("native editor lost admitted text")
      return { buffer, view }
    })
    const display = OptimizedBuffer.create(110, 5, "wcwidth")
    const label = TextBuffer.create("wcwidth")
    const labelView = TextBufferView.create(label)
    // Distinct URLs and combining sequences exercise intern-key retirement.
    lib.textBufferSetStyledText(label.ptr, [{ text: `link e\u0301界 ${cycle}`, link: { url: `https://example.invalid/${cycle}` } }])
    display.drawTextBuffer(labelView, 0, 0)
    display.clear()
    labelView.destroy(); label.destroy(); display.destroy()
    // Deliberately not stack order: an arena cannot reclaim these owners.
    for (const { buffer, view } of views) { view.destroy(); buffer.destroy() }
    if (cycle >= 31) snapshots.push({ cycle, arenaBytes: lib.getArenaAllocatedBytes(), ...lib.getAllocatorStats() })
  }
  const first = snapshots[0]!
  const settled = snapshots.every(sample => sample.arenaBytes === first.arenaBytes && sample.activeAllocations === first.activeAllocations
    && (!sample.requestedBytesValid || sample.totalRequestedBytes === first.totalRequestedBytes))
  return { schemaVersion: 1, library, librarySha256: createHash("sha256").update(readFileSync(library)).digest("hex"), buildOptions: lib.getBuildOptions(), cycles: 256, interleavedViews: 16, settled, snapshots }
}

if (import.meta.main) {
  if (Bun.argv.length !== 3) throw new Error("usage: bun native-lifetime-probe.ts ABSOLUTE_LIBRARY")
  const report = probeNativeLifetimes(Bun.argv[2]!)
  console.log(JSON.stringify(report))
  if (!report.settled) process.exitCode = 1
}
