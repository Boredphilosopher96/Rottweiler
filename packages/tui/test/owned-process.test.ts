import { afterEach, expect, test } from "bun:test"
import { access, readFile, rm, writeFile } from "node:fs/promises"
import { join } from "node:path"
import { TestProcessScope } from "./support/owned-process"

const owners: TestProcessScope[] = []
afterEach(async () => {
  for (const owner of owners.splice(0)) await owner.close()
}, 15_000)
async function scope(): Promise<TestProcessScope> {
  const owner = await TestProcessScope.create("rw-owned-process-")
  owners.push(owner)
  return owner
}

test("test process owner drains both pipes and reports a nonzero exit", async () => {
  const owner = await scope()
  const result = await owner.run([process.execPath, "-e", 'console.log("out");console.error("err");process.exit(7)'], { timeoutMs: 2_000 })
  expect(result).toEqual({ code: 7, stdout: "out\n", stderr: "err\n" })
})

for (const mode of ["timeout", "output"] as const) {
  test(`test process owner reaps a child before reporting ${mode} failure`, async () => {
    const owner = await scope()
    const pidFile = join(owner.directory, "pid")
    const behavior = mode === "timeout" ? 'setInterval(() => {}, 100)' : 'setInterval(() => {process.stdout.write("x".repeat(1024));process.stderr.write("x".repeat(1024))}, 1)'
    const program = `import {writeFileSync} from "node:fs";writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));${behavior}`
    await expect(owner.run([process.execPath, "-e", program], {
      timeoutMs: 2_000, maxOutputBytes: 2_048,
    })).rejects.toThrow(mode === "timeout" ? "exceeded 2s" : "output")
    const pid = Number(await readFile(pidFile, "utf8"))
    expect(() => process.kill(pid, 0)).toThrow()
  }, 15_000)
}

test("synchronous preload descendants finish before the directory owner closes", async () => {
  const owner = await scope()
  const pidFile = join(owner.directory, "preload-pid")
  const python = `import os,time;open(${JSON.stringify(pidFile)},'w').write(str(os.getpid()));time.sleep(.2)`
  const program = `const child = Bun.spawnSync(["python3", "-c", ${JSON.stringify(python)}]);if(child.exitCode!==0)throw new Error("preload failed")`
  await owner.run([process.execPath, "-e", program], { timeoutMs: 2_000 })
  const pid = Number(await readFile(pidFile, "utf8"))
  expect(() => process.kill(pid, 0)).toThrow()
  await owner.close()
  await expect(access(owner.directory)).rejects.toThrow()
})

test("cleanup during a pending proof awaits its physical owner and prevents another launch", async () => {
  const owner = await scope()
  const operation = owner.run([process.execPath, "-e", 'await Bun.sleep(250);console.log("settled")'], { timeoutMs: 2_000 })
  const closure = owner.close()
  expect(() => owner.run([process.execPath, "-e", 'process.exit(0)'], { timeoutMs: 2_000 })).toThrow("closed")
  expect((await operation).stdout).toBe("settled\n")
  await closure
  await expect(access(owner.directory)).rejects.toThrow()
})

test("a killed bridge cannot acknowledge closure or authorize deleting its directory", async () => {
  const owner = await TestProcessScope.create("rw-unsettled-process-")
  const pidFile = join(owner.directory, "pid")
  const program = `await Bun.write(${JSON.stringify(pidFile)}, String(process.pid));process.kill(process.ppid,"SIGKILL");process.exit(0)`
  await expect(owner.run([process.execPath, "-e", program], { timeoutMs: 2_000 })).rejects.toThrow("UNSETTLED")
  expect(() => owner.run([process.execPath, "-e", "process.exit(0)"], { timeoutMs: 2_000 })).toThrow("UNSETTLED")
  await expect(owner.close()).rejects.toThrow(`retained ${owner.directory}`)
  await access(pidFile)
  // The deliberate failure fixture independently observes its own child's exit.
  // Production cleanup never substitutes this for the missing acknowledgement.
  const pid = Number(await readFile(pidFile, "utf8"))
  const until = performance.now() + 5_000
  while (performance.now() < until) {
    try { process.kill(pid, 0) } catch { await rm(owner.directory, { recursive: true }); return }
    await Bun.sleep(10)
  }
  throw new Error(`UNSETTLED negative fixture retained ${owner.directory}`)
}, 10_000)


test("Bun test timeout cleanup awaits its pending supervisor before deleting scratch", async () => {
  const owner = await scope()
  const fixture = join(owner.directory, "timeout.test.ts")
  const pidFile = join(owner.directory, "timeout.pid")
  const doneFile = join(owner.directory, "closed.json")
  const child = `await Bun.write(${JSON.stringify(pidFile)},String(process.pid));await Bun.sleep(500)`
  await writeFile(fixture, `
import {test,afterEach} from "bun:test";
import {TestProcessScope} from ${JSON.stringify(new URL("./support/owned-process.ts", import.meta.url).pathname)};
const owner=await TestProcessScope.create("rw-timed-test-");
afterEach(async()=>{await owner.close();await Bun.write(${JSON.stringify(doneFile)},JSON.stringify({directory:owner.directory}))},15000);
test("deadline",async()=>{await owner.run([process.execPath,"-e",${JSON.stringify(child)}],{timeoutMs:2000})},100);
`)
  const result = await owner.run([process.execPath, "test", fixture], { timeoutMs: 10_000 })
  expect(result.code).toBe(1)
  const closed = JSON.parse(await readFile(doneFile, "utf8").catch(error => { throw new Error(result.stderr + result.stdout, {cause:error}) }))
  await expect(access(closed.directory)).rejects.toThrow()
  const pid = Number(await readFile(pidFile, "utf8"))
  expect(() => process.kill(pid, 0)).toThrow()
}, 20_000)


test("deadline during a synchronous preload never reports settlement while its group remains", async () => {
  const owner = await TestProcessScope.create("rw-blocked-preload-")
  const pidFile = join(owner.directory, "pid")
  const heartbeat = join(owner.directory, "heartbeat")
  const python = `import os,time
open(${JSON.stringify(pidFile)},"w").write(str(os.getpid()))
while True:
 open(${JSON.stringify(heartbeat)},"w").write(str(time.monotonic_ns()))
 time.sleep(.01)`
  const program = `Bun.spawnSync(["python3","-c",${JSON.stringify(python)}])`
  let failure = ""
  try { await owner.run([process.execPath, "-e", program], { timeoutMs: 2_000 }) }
  catch (error) { failure = String(error) }
  expect(failure).toMatch(/exceeded 2s|UNSETTLED/)
  const stopped = await readFile(heartbeat, "utf8")
  await Bun.sleep(100)
  expect(await readFile(heartbeat, "utf8")).toBe(stopped)
  if (failure.includes("UNSETTLED")) {
    // An unreaped orphan zombie is not group disappearance. The failure fixture
    // preserves that evidence rather than treating stopped execution as closure.
    await expect(owner.close()).rejects.toThrow(`retained ${owner.directory}`)
    await access(pidFile)
    console.error(`Expected unproven preload settlement retained ${owner.directory}`)
  } else {
    const pid = Number(await readFile(pidFile, "utf8"))
    expect(() => process.kill(pid, 0)).toThrow()
    await owner.close()
    await expect(access(owner.directory)).rejects.toThrow()
  }
}, 15_000)
