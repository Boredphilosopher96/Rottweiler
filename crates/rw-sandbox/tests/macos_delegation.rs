#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]
//! Probe delegation through the inherited bootstrap namespace and a real Unix socket.
use rw_sandbox::{
    EgressPolicy, NetworkPolicy, SandboxPolicy, SupervisedEgressProxy, shell_launch_plan,
};
use std::{ffi::OsString, os::unix::net::UnixListener, path::Path, process::Command};

const PROBE: &str = r"
import ctypes, errno, os, socket, sys
lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
bootstrap = ctypes.c_uint.in_dll(lib, 'bootstrap_port').value
service = ctypes.c_uint()
status = lib.bootstrap_look_up(bootstrap, b'com.apple.coreservices.launchservicesd', ctypes.byref(service))
if sys.argv[1] == 'baseline':
    assert status == 0, ('baseline service lookup', status)
    socket.socket(socket.AF_UNIX).connect(sys.argv[2])
    print('baseline bootstrap lookup and Unix connection succeed')
    sys.exit(0)
assert status == 1100, ('inherited bootstrap lookup must return BOOTSTRAP_NOT_PRIVILEGED', status)
if sys.argv[3] != '0':
    socket.create_connection(('127.0.0.1', int(sys.argv[3])), timeout=2).close()
try:
    socket.socket(socket.AF_UNIX).connect(sys.argv[2])
except OSError as error:
    assert error.errno in (errno.EPERM, errno.EACCES), error
else:
    raise AssertionError('Unix broker connection escaped policy')
# Inspect the kernel policy without sending an AppleEvent to another application.
assert lib.sandbox_check(os.getpid(), b'appleevent-send', 0) != 0
assert lib.sandbox_check(os.getpid(), b'mach-priv-task-port', 0) != 0
assert lib.sandbox_check(os.getpid(), b'ipc-posix-shm-write-create', 0) != 0
print('inherited bootstrap lookup and Unix connection denied; AppleEvent/task/IPC policy denied')
";

#[test]
fn single_process_policy_denies_service_delegation_in_both_network_modes() {
    let directory = tempfile::tempdir().expect("private test directory");
    let socket = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).expect("owned broker socket");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let resolved = Command::new("python3")
        .args([
            "-c",
            "import os,sys; print(os.path.realpath(sys.executable))",
        ])
        .output()
        .expect("resolve actual interpreter before sandboxing");
    assert!(resolved.status.success());
    let interpreter = String::from_utf8(resolved.stdout).expect("interpreter path");
    let interpreter = Path::new(interpreter.trim());
    assert!(interpreter.is_absolute());
    let baseline = Command::new(interpreter)
        .args(["-c", PROBE, "baseline"])
        .arg(&socket)
        .output()
        .expect("baseline probe");
    assert!(
        baseline.status.success(),
        "{}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let _ = listener.accept().expect("baseline connection observed");
    let proxy =
        SupervisedEgressProxy::start(EgressPolicy::new(std::iter::empty::<&str>())).expect("proxy");
    for network in [
        NetworkPolicy::Deny,
        NetworkPolicy::PolicyProxy {
            port: proxy.address().port(),
            relay_path: None,
        },
    ] {
        let allowed_port = match &network {
            NetworkPolicy::Deny => 0,
            NetworkPolicy::PolicyProxy { port, .. } => *port,
        };
        let policy = SandboxPolicy::new([directory.path()], network)
            .expect("policy")
            .without_process_creation();
        let args = [
            OsString::from("-c"),
            OsString::from(PROBE),
            OsString::from("restricted"),
            socket.as_os_str().to_owned(),
            OsString::from(allowed_port.to_string()),
        ];
        let plan =
            shell_launch_plan(&policy, Path::new("rw"), interpreter, &args).expect("launch plan");
        let output = Command::new(plan.program)
            .args(plan.args)
            .output()
            .expect("sandbox probe");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}
