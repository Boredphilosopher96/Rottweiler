import { afterEach, expect, test } from "bun:test"
import { access, readFile, writeFile } from "node:fs/promises"
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

test("a killed bridge denies acknowledgement until its failure fixture proves retirement", async () => {
  const outer = await scope()
  const controller = `
import {access,readFile,rm} from "node:fs/promises";
import {TestProcessScope} from ${JSON.stringify(new URL("./support/owned-process.ts", import.meta.url).pathname)};
const owner=await TestProcessScope.create("rw-unsettled-process-");
const pidFile=owner.directory+"/pid";
const program="await Bun.write("+JSON.stringify(pidFile)+",String(process.pid));process.kill(process.ppid,'SIGKILL');process.exit(0)";
let rejected=false;
try{await owner.run([process.execPath,"-e",program],{timeoutMs:2000})}catch(error){rejected=String(error).includes("UNSETTLED")}
if(!rejected)throw new Error("killed bridge falsely acknowledged");
try{await owner.close();throw new Error("unproven directory was deleted")}catch(error){if(!String(error).includes("retained"))throw error}
await access(pidFile);
const pid=Number(await readFile(pidFile,"utf8"));
const until=Date.now()+5000;
let absent=false;
while(Date.now()<until){try{process.kill(pid,0)}catch{absent=true;break}await Bun.sleep(10)}
if(!absent)throw new Error("UNSETTLED failure fixture retained "+owner.directory);
// Its fixed child program creates no descendants and exits immediately after
// killing the bridge. The controller observed worker exit and this child exit.
await rm(owner.directory,{recursive:true});
console.log("negative fixture physically retired");
`
  const result = await outer.run([process.execPath, "-e", controller], { timeoutMs: 10_000 })
  expect(result.code).toBe(0)
  expect(result.stdout).toContain("negative fixture physically retired")
}, 15_000)


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
    await expect(owner.close()).rejects.toThrow(`retained ${owner.directory}`)
    throw new Error(`UNSETTLED preload fixture cannot qualify physical closure: ${owner.directory}`)
  }
  const pid = Number(await readFile(pidFile, "utf8"))
  expect(() => process.kill(pid, 0)).toThrow()
  await owner.close()
  await expect(access(owner.directory)).rejects.toThrow()
}, 15_000)

test("VM death closes its lifeline and the Python owner reaps native work", async () => {
  const owner = await scope()
  const pidFile = join(owner.directory, "orphan.pid")
  const directoryFile = join(owner.directory, "orphan.directory")
  const native = `import os,time;open(${JSON.stringify(pidFile)},'w').write(str(os.getpid()));time.sleep(30)`
  const victim = `
import {TestProcessScope} from ${JSON.stringify(new URL("./support/owned-process.ts", import.meta.url).pathname)};
const owner=await TestProcessScope.create("rw-vm-loss-");
await Bun.write(${JSON.stringify(directoryFile)}, owner.directory);
await owner.run(["python3","-c",${JSON.stringify(native)}],{timeoutMs:5000});
`
  // This controller remains alive while its child VM dies. It explicitly waits
  // the retained Python/native proof before its own outer group may be settled.
  const controller = `
import {readFile,rm} from "node:fs/promises";
const child=Bun.spawn([process.execPath,"-e",${JSON.stringify(victim)}],{stdin:"ignore",stdout:"ignore",stderr:"ignore"});
const deadline=Date.now()+8000;
async function file(path){while(Date.now()<deadline){try{return await readFile(path,"utf8")}catch{}await Bun.sleep(5)}throw new Error("missing physical evidence: "+path)}
async function absent(pid){while(Date.now()<deadline){try{process.kill(pid,0)}catch{return}await Bun.sleep(5)}throw new Error("process still present: "+pid)}
let result;
try{
 const pid=Number(await file(${JSON.stringify(pidFile)}));
 const directory=await file(${JSON.stringify(directoryFile)});
 child.kill("SIGKILL");await child.exited;
 result=JSON.parse(await file(directory+"/process.result.json"));
 if(result.settled!==true)throw new Error("UNSETTLED "+directory);
 await absent(result.supervisor_pid);await absent(pid);
 await rm(directory,{recursive:true});
 if(!String(result.error).includes("ScopeCancelled"))throw new Error("parent loss did not cancel native work: "+result.error);
 console.log("physical parent-loss settlement");
}finally{if(child.exitCode===null){child.kill("SIGTERM");await child.exited}}
`
  const result = await owner.run([process.execPath, "-e", controller], { timeoutMs: 12_000 })
  expect(result.code).toBe(0)
  expect(result.stdout).toContain("physical parent-loss settlement")
}, 20_000)
