import { dlopen, FFIType as T, ptr, read } from "bun:ffi"
import { realpathSync, existsSync } from "node:fs"
if (process.platform !== "linux" || process.arch !== "arm64") throw new Error("Linux arm64 diagnostic only")
const libc = dlopen("libc.so.6", {
  syscall: {args: [T.i64,T.i64,T.i64,T.i64,T.i64,T.i64,T.i64], returns: T.i64},
  open: {args: [T.ptr,T.i32],returns:T.i32},
  close: {args:[T.i32],returns:T.i32},
  prctl: {args:[T.i32,T.i64,T.i64,T.i64,T.i64],returns:T.i32},
  execv: {args:[T.ptr,T.ptr],returns:T.i32},
  __errno_location: {args:[],returns:T.ptr},
})
function call(n: number, a: number|bigint=0,b: number|bigint=0,c: number|bigint=0): bigint {
  const value=libc.symbols.syscall(n,a,b,c,0,0,0)
  if(value<0)throw new Error(`syscall ${n} errno ${read.i32(libc.symbols.__errno_location())}`)
  return value
}
const abi=call(444,0,0,1)
const all=(1<<15)-1, readAccess=1|4|8
const attr=new BigUint64Array([BigInt(all)])
const ruleset=call(444,ptr(attr),8,0)
function grant(path:string,access:number):void {
  if(!existsSync(path))return
  const name=Buffer.from(`${realpathSync(path)}\0`)
  const fd=libc.symbols.open(ptr(name),0x200000|0x80000)
  if(fd<0)throw new Error(`open ${path}`)
  const rule=new Uint8Array(12),view=new DataView(rule.buffer)
  view.setBigUint64(0,BigInt(access),true); view.setInt32(8,fd,true)
  call(445,ruleset,1,ptr(rule));libc.symbols.close(fd)
}
for(const path of ["/usr","/bin","/sbin","/lib","/lib64","/etc","/proc/self","/sys/devices/system/cpu","/sys/fs/cgroup","/tmp"])grant(path,readAccess)
for(const path of ["/dev/null","/dev/zero","/dev/random","/dev/urandom"])grant(path,1|4)
grant("/home/plugin-user/packages/example",readAccess)
grant("/tmp/source-output",all)
if(libc.symbols.prctl(38,1,0,0,0)!==0)throw new Error("no_new_privs")
call(446,ruleset,0,0)
console.log(JSON.stringify({bun:Bun.version,arch:process.arch,landlockABI:String(abi),root:"/home/plugin-user/packages/example",ancestorGrant:false}))
const args=process.argv.slice(2).map(value=>Buffer.from(`${value}\0`))
const pointers=new BigUint64Array([...args.map(value=>BigInt(ptr(value))),0n])
if(args[0]===undefined)throw new Error("executable required")
libc.symbols.execv(ptr(args[0]),ptr(pointers))
throw new Error(`execv errno ${read.i32(libc.symbols.__errno_location())}`)
