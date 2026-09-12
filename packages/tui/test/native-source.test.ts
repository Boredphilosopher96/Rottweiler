import { expect, test } from "bun:test"
import { resolve } from "node:path"
import { EditBuffer, EditorView, resolveRenderLib } from "@opentui/core"

const cwd = resolve(import.meta.dir, "..")
function child(program: string, library?: string) {
  const env = { ...process.env }
  if (library === undefined) delete env.ROTTWEILER_OPENTUI_LIBRARY
  else env.ROTTWEILER_OPENTUI_LIBRARY = library
  return Bun.spawnSync([process.execPath, "-e", program], { cwd, env, stdout: "pipe", stderr: "pipe" })
}

test("source renderer reclaims interleaved native views with the verified allocator", () => {
  const buffers = Array.from({ length: 8 }, () => EditBuffer.create("wcwidth"))
  const views = buffers.map(buffer => {
    buffer.setText("wide 界 and combining é ".repeat(200))
    return EditorView.create(buffer, 80, 20)
  })
  for (const index of [0, 2, 4, 6, 1, 3, 5, 7]) {
    views[index]!.destroy()
    buffers[index]!.destroy()
  }
  expect(resolveRenderLib().getArenaAllocatedBytes()).toBe(0)
})

test("source native initialization refuses missing or unreceipted artifacts", () => {
  for (const library of [undefined, "/tmp/no-rottweiler-native-receipt/libopentui.dylib"]) {
    const result = child('const {EditBuffer}=await import("@opentui/core"); EditBuffer.create("wcwidth")', library)
    expect(result.exitCode).not.toBe(0)
    expect(result.stderr.toString()).toContain("Prepare and export ROTTWEILER_OPENTUI_LIBRARY")
  }
})

test("source virtual module selects the exact verified artifact", () => {
  const result = child('console.log((await import("rottweiler-opentui-native")).default)', process.env.ROTTWEILER_OPENTUI_LIBRARY)
  expect(result.exitCode).toBe(0)
  expect(result.stdout.toString().trim()).toBe(process.env.ROTTWEILER_OPENTUI_LIBRARY ?? "")
})

test("source-plugin role in the TUI directory needs no native artifact", () => {
  const result = child('import {plugin} from "bun"; plugin({name:"deny-renderer",setup(build){build.module("@opentui/core",()=>{throw new Error("source plugin imported the renderer")});build.module("@rottweiler/plugin",()=>{throw new Error("source version imported SDK execution")})}}); const {runJavaScriptHost}=await import("../js-host/src/index.ts"); await runJavaScriptHost(["source-plugin","version"])')
  expect(result.stderr.toString()).toBe("")
  expect(result.exitCode).toBe(0)
  expect(JSON.parse(result.stdout.toString())).toHaveProperty("abi")
})
