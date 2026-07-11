#![cfg(target_os = "linux")]
#![allow(clippy::expect_used)]

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use rw_sandbox::{
    EgressPolicy, NetworkPolicy, SandboxCapability, SandboxPolicy, SandboxSupport,
    SupervisedEgressProxy, probe_policy_egress,
};
use tempfile::tempdir;

const REQUIRE_LINUX_SANDBOX_ENV: &str = "ROTTWEILER_REQUIRE_LINUX_SANDBOX";

#[test]
fn policy_egress_probe_distinguishes_container_or_kernel_refusal() {
    let probe = probe_policy_egress();
    if std::env::var_os("ROTTWEILER_EXPECT_EGRESS_UNAVAILABLE").is_some() {
        assert_eq!(probe.support, SandboxSupport::Unavailable);
        assert!(
            probe
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("policy egress is refused"))
        );
    } else if probe.support == SandboxSupport::Unavailable {
        assert!(
            probe
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("policy egress is refused")),
            "{probe:?}"
        );
    } else {
        assert_eq!(probe.support, SandboxSupport::Enforced);
    }
}

#[test]
fn netns_routes_only_the_inner_policy_proxy_and_blocks_direct_and_unix_bypasses() {
    if !sandbox_available(&probe_policy_egress(), "policy egress isolation") {
        return;
    }
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("target");
    let target_address = target.local_addr().expect("target address");
    let target_worker = thread::spawn(move || {
        let (mut stream, _) = target.accept().expect("target accept");
        let mut request = [0_u8; 2048];
        let length = stream.read(&mut request).expect("target request");
        let request = String::from_utf8_lossy(&request[..length]);
        assert!(
            request.starts_with("GET /allowed HTTP/1.1\r\n"),
            "{request:?}"
        );
        assert!(request.contains(&format!(
            "\r\nHost: localhost:{}\r\n",
            target_address.port()
        )));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
            .expect("target response");
    });
    let proxy = SupervisedEgressProxy::start(
        EgressPolicy::new(["localhost"]).with_private_destinations(true),
    )
    .expect("policy proxy");
    let relay_path = proxy.relay_path().expect("Linux relay path").to_path_buf();
    let workspace = tempdir().expect("workspace");
    let policy = SandboxPolicy::new(
        [workspace.path()],
        NetworkPolicy::PolicyProxy {
            port: proxy.address().port(),
            relay_path: Some(relay_path),
        },
    )
    .expect("policy");
    let script = r"
import errno, socket, sys
proxy_port = PROXY_PORT
target_port = TARGET_PORT
s = socket.create_connection(('127.0.0.1', proxy_port), timeout=3)
s.sendall(('GET http://localhost:%d/allowed HTTP/1.1\r\nHost: localhost:%d\r\nConnection: close\r\n\r\n' % (target_port, target_port)).encode())
response = b''
while True:
    part = s.recv(4096)
    if not part: break
    response += part
if not response.endswith(b'ok'): sys.exit(90)
for destination in [('127.0.0.1', target_port), ('1.1.1.1', proxy_port)]:
    direct = socket.socket(socket.AF_INET, socket.SOCK_STREAM); direct.settimeout(1)
    try: direct.connect(destination)
    except OSError: pass
    else: sys.exit(91)
try: socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
except OSError as error:
    if error.errno not in (errno.EPERM, errno.EACCES): sys.exit(92)
else: sys.exit(93)
try:
    binder = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    binder.bind(('127.0.0.1', 0))
except OSError as error:
    if error.errno not in (errno.EPERM, errno.EACCES): sys.exit(94)
else: sys.exit(95)
"
    .replace("PROXY_PORT", &proxy.address().port().to_string())
    .replace("TARGET_PORT", &target_address.port().to_string());
    let args = [OsString::from("-c"), OsString::from(script)];
    let status = test_helper_command(&policy, Path::new("/usr/bin/python3"), &args)
        .status()
        .expect("Linux policy command");
    assert!(status.success(), "Linux policy probe exited {status}");
    target_worker.join().expect("target worker");
}

#[test]
fn deny_mode_uses_an_empty_netns_and_blocks_udp_raw_send_and_io_uring_syscalls() {
    if !sandbox_available(&rw_sandbox::probe(), "deny-mode network isolation") {
        return;
    }
    let workspace = tempdir().expect("workspace");
    let policy = SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny).expect("deny policy");
    let script = r"
import ctypes, errno, socket, sys

def must_be_denied(result, name):
    if result != -1 or ctypes.get_errno() not in (errno.EPERM, errno.EACCES):
        sys.exit(name)

for family, kind, code in [
    (socket.AF_INET, socket.SOCK_DGRAM, 81),
    (socket.AF_INET6, socket.SOCK_DGRAM, 82),
    (socket.AF_PACKET, socket.SOCK_RAW, 83),
]:
    try: socket.socket(family, kind)
    except OSError as error:
        if error.errno not in (errno.EPERM, errno.EACCES): sys.exit(code)
    else: sys.exit(code)

libc = ctypes.CDLL(None, use_errno=True)
ctypes.set_errno(0)
must_be_denied(libc.sendto(-1, None, 0, 0, None, 0), 84)
ctypes.set_errno(0)
must_be_denied(libc.sendmsg(-1, None, 0), 85)
ctypes.set_errno(0)
must_be_denied(libc.syscall(425, 1, None), 86)
";
    let args = [OsString::from("-c"), OsString::from(script)];
    let status = test_helper_command(&policy, Path::new("/usr/bin/python3"), &args)
        .status()
        .expect("Linux deny command");
    assert!(status.success(), "Linux deny canary exited {status}");
}

#[test]
fn pid_namespace_kills_setsid_descendants_before_launch_returns() {
    if !sandbox_available(&rw_sandbox::probe(), "PID isolation") {
        return;
    }
    let workspace = tempdir().expect("workspace");
    let policy = SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny).expect("deny policy");
    let late_write = workspace.path().join("late-write");
    let script = "setsid /bin/sh -c 'sleep 0.2; printf escaped > \"$1\"' &";
    let args = [
        OsString::from("-c"),
        OsString::from(script),
        OsString::from("daemon-canary"),
        late_write.as_os_str().to_owned(),
    ];
    let status = test_helper_command(&policy, Path::new("/bin/sh"), &args)
        .status()
        .expect("Linux daemon command");
    assert!(status.success(), "Linux daemon canary exited {status}");
    thread::sleep(Duration::from_millis(500));
    assert!(
        !late_write.exists(),
        "setsid descendant survived the command PID namespace"
    );
}

#[test]
fn approved_sandboxed_write_outside_workspace_is_denied_at_the_syscall() {
    if !sandbox_available(&rw_sandbox::probe(), "filesystem isolation") {
        return;
    }
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside workspace");
    let allowed_path = workspace.path().join("allowed-write");
    let denied_path = outside.path().join("denied-write");
    let policy = SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny).expect("deny policy");
    let script = r"
import errno, os, sys

allowed = sys.argv[1]
denied = sys.argv[2]
descriptor = os.open(allowed, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
os.write(descriptor, b'allowed')
os.close(descriptor)
try:
    os.open(denied, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
except OSError as error:
    if error.errno not in (errno.EPERM, errno.EACCES):
        sys.exit(81)
else:
    sys.exit(82)
";
    let args = [
        OsString::from("-c"),
        OsString::from(script),
        allowed_path.as_os_str().to_owned(),
        denied_path.as_os_str().to_owned(),
    ];
    let status = test_helper_command(&policy, Path::new("/usr/bin/python3"), &args)
        .status()
        .expect("Linux filesystem command");
    assert!(status.success(), "Linux filesystem canary exited {status}");
    assert_eq!(
        std::fs::read(&allowed_path).expect("allowed write result"),
        b"allowed"
    );
    assert!(!denied_path.exists(), "outside-workspace file was created");
}

#[test]
fn two_write_roots_do_not_grant_their_parent_or_sibling() {
    if !sandbox_available(&rw_sandbox::probe(), "multi-root filesystem isolation") {
        return;
    }
    let container = tempdir().expect("root container");
    let first = container.path().join("first");
    let second = container.path().join("second");
    let sibling = container.path().join("sibling");
    std::fs::create_dir_all(&first).expect("first root");
    std::fs::create_dir_all(&second).expect("second root");
    std::fs::create_dir_all(&sibling).expect("sibling");
    let policy =
        SandboxPolicy::new([&first, &second], NetworkPolicy::Deny).expect("multi-root policy");
    let allowed_first = first.join("write-one");
    let allowed_second = second.join("write-two");
    let denied_parent = container.path().join("parent-write");
    let denied_sibling = sibling.join("sibling-write");
    let script = r"
import errno, os, sys

for path in sys.argv[1:3]:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.write(descriptor, b'allowed')
    os.close(descriptor)
for index, path in enumerate(sys.argv[3:], start=83):
    try:
        os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except OSError as error:
        if error.errno not in (errno.EPERM, errno.EACCES):
            sys.exit(index)
    else:
        sys.exit(index)
";
    let args = [
        OsString::from("-c"),
        OsString::from(script),
        allowed_first.as_os_str().to_owned(),
        allowed_second.as_os_str().to_owned(),
        denied_parent.as_os_str().to_owned(),
        denied_sibling.as_os_str().to_owned(),
    ];
    let status = test_helper_command(&policy, Path::new("/usr/bin/python3"), &args)
        .status()
        .expect("Linux multi-root command");
    assert!(status.success(), "Linux multi-root canary exited {status}");
    assert_eq!(
        std::fs::read(&allowed_first).expect("first result"),
        b"allowed"
    );
    assert_eq!(
        std::fs::read(&allowed_second).expect("second result"),
        b"allowed"
    );
    assert!(!denied_parent.exists(), "parent path became writable");
    assert!(!denied_sibling.exists(), "sibling path became writable");
}

fn sandbox_available(capability: &SandboxCapability, requirement: &str) -> bool {
    if capability.support == SandboxSupport::Enforced {
        return true;
    }
    let warning = capability
        .warning
        .as_deref()
        .unwrap_or("sandbox capability unavailable");
    assert!(
        std::env::var_os(REQUIRE_LINUX_SANDBOX_ENV).is_none(),
        "{requirement} is required by {REQUIRE_LINUX_SANDBOX_ENV}, but the host reported: {warning}"
    );
    eprintln!("skipping {requirement}: {warning}");
    false
}

fn test_helper_command(policy: &SandboxPolicy, program: &Path, args: &[OsString]) -> Command {
    let mut command = Command::new("/usr/bin/unshare");
    command.args([
        "--user",
        "--map-current-user",
        "--net",
        "--pid",
        "--fork",
        "--kill-child",
        "--",
    ]);
    command
        .arg(env!("CARGO_BIN_EXE_rw-sandbox-helper"))
        .arg(rw_sandbox::HELPER_ARG)
        .arg(serde_json::to_string(policy).expect("serialize policy"))
        .arg(program)
        .args(args);
    command
}
