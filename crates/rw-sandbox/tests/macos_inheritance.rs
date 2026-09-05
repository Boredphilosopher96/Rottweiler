#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]
//! Seed an owned effect port before the real sandbox/bootstrap exec chain.
use rw_sandbox::{NetworkPolicy, SandboxPolicy, shell_launch_plan};
use std::{ffi::OsString, path::Path, process::Command};

const CHILD: &str = r"
import ctypes, os, sys
lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
task = ctypes.c_uint.in_dll(lib, 'mach_task_self_').value
ports = ctypes.POINTER(ctypes.c_uint)(); count = ctypes.c_uint()
assert lib.mach_ports_lookup(task, ctypes.byref(ports), ctypes.byref(count)) == 0
mode = sys.argv[1]
if mode == 'baseline':
    assert any(ports[:count.value]), 'baseline registered port missing'
else:
    assert not any(ports[:count.value]), 'registered effect port remains'
    assert ctypes.c_uint.in_dll(lib, 'bootstrap_port').value == 0
    target = ctypes.c_uint()
    assert lib.task_name_for_pid(task, os.getppid(), ctypes.byref(target)) != 0
    assert target.value == 0
    assert lib.task_for_pid(task, os.getppid(), ctypes.byref(target)) != 0
    assert target.value == 0
masks = (ctypes.c_uint * 32)(); handlers = (ctypes.c_uint * 32)()
behaviors = (ctypes.c_int * 32)(); flavors = (ctypes.c_int * 32)(); length = ctypes.c_uint(32)
assert lib.task_get_exception_ports(task, 64, masks, ctypes.byref(length), handlers, behaviors, flavors) == 0
assert bool(any(handlers[:length.value])) == (mode == 'baseline')
if mode == 'baseline':
    message = (ctypes.c_uint * 6)(19, 24, ctypes.c_uint.in_dll(lib, 'bootstrap_port').value, 0, 0, 1122)
    assert lib.mach_msg(ctypes.byref(message), 17, 24, 0, 0, 0, 0) == 0
print(mode, 'application discovery roots', bool(any(ports[:count.value])))
";

const PARENT: &str = r"
import ctypes, json, os, subprocess, sys, threading
lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
task = ctypes.c_uint.in_dll(lib, 'mach_task_self_').value
port = ctypes.c_uint()
assert lib.mach_port_allocate(task, 1, ctypes.byref(port)) == 0
assert lib.mach_port_insert_right(task, port.value, port.value, 20) == 0
assert lib.mach_ports_register(task, ctypes.byref(port), 1) == 0
assert lib.task_set_exception_ports(task, 64, port.value, 1, 0) == 0
old_bootstrap = ctypes.c_uint()
assert lib.task_get_special_port(task, 4, ctypes.byref(old_bootstrap)) == 0
stop = threading.Event(); receipts = []; failures = []
def serve():
    while True:
        buffer = (ctypes.c_uint * 16384)()
        result = lib.mach_msg(ctypes.byref(buffer), 258, 0, ctypes.sizeof(buffer), port.value, 100, 0)
        if result != 0:
            if stop.is_set(): break
            continue
        if buffer[5] == 1122: receipts.append(1122)
        if buffer[2]:
            # Refuse bootstrap startup queries; never delegate to real services.
            reply = (ctypes.c_uint * 9)(buffer[0] & 255, 36, buffer[2], 0, 0, buffer[5] + 100, 0, 1, 1100)
            result = lib.mach_msg(ctypes.byref(reply), 17, 36, 0, 0, 0, 0)
            if result != 0: failures.append(result)
server = threading.Thread(target=serve); server.start()
assert lib.task_set_special_port(task, 4, port.value) == 0
try:
    for mode, command in json.loads(sys.argv[1]):
        result = subprocess.run(command, capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, (mode, result.stdout, result.stderr)
        print(result.stdout.strip())
finally:
    assert lib.task_set_special_port(task, 4, old_bootstrap.value) == 0
    stop.set(); server.join(timeout=2); assert not server.is_alive()
    assert not failures, ('bootstrap refusal reply failures', failures)
    assert receipts == [1122], ('actual parent effect receipts', receipts)
    print('parent received only baseline effect')
    assert lib.mach_ports_register(task, None, 0) == 0
    assert lib.task_set_exception_ports(task, 64, 0, 1, 0) == 0
    assert lib.mach_port_destroy(task, port.value) == 0
";

#[test]
fn real_worker_revokes_inherited_bootstrap_effect_authority() {
    let resolved = Command::new("python3")
        .args([
            "-c",
            "import os,sys; print(os.path.realpath(sys.executable))",
        ])
        .output()
        .expect("resolve interpreter");
    assert!(resolved.status.success());
    let interpreter = String::from_utf8(resolved.stdout).expect("interpreter path");
    let interpreter = interpreter.trim();
    let directory = tempfile::tempdir().expect("scratch");
    let policy = SandboxPolicy::new([directory.path()], NetworkPolicy::Deny)
        .expect("policy")
        .without_process_creation();
    let child_args = [
        OsString::from("-c"),
        OsString::from(CHILD),
        OsString::from("worker"),
    ];
    let plan = shell_launch_plan(
        &policy,
        Path::new(env!("CARGO_BIN_EXE_rw-sandbox-helper")),
        Path::new(interpreter),
        &child_args,
    )
    .expect("worker plan");
    let mut worker = vec![plan.program.to_string_lossy().into_owned()];
    worker.extend(
        plan.args
            .iter()
            .map(|value| value.to_string_lossy().into_owned()),
    );
    let mut baseline = worker.clone();
    let helper_index = baseline
        .iter()
        .position(|value| value == env!("CARGO_BIN_EXE_rw-sandbox-helper"))
        .expect("helper in launch plan");
    baseline.drain(helper_index..helper_index + 2);
    *baseline.last_mut().expect("mode") = "baseline".to_owned();
    let commands =
        serde_json::to_string(&[("baseline", baseline), ("worker", worker)]).expect("commands");
    let output = Command::new(interpreter)
        .args(["-c", PARENT, &commands])
        .output()
        .expect("Mach inheritance probe");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
}
