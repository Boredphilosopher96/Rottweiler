#![cfg(target_os = "linux")]
#![allow(clippy::expect_used)]

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixDatagram;
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
const UNIX_PAIR_CHILD_ENV: &str = "ROTTWEILER_SANDBOX_UNIX_PAIR_CHILD";
const UNIX_DGRAM_TARGET_ENV: &str = "ROTTWEILER_SANDBOX_UNIX_DGRAM_TARGET";
const FILE_READ_FIXTURE_ENV: &str = "ROTTWEILER_SANDBOX_FILE_READ_FIXTURE";
const FILE_READ_OUTPUT_ENV: &str = "ROTTWEILER_SANDBOX_FILE_READ_OUTPUT";
const DEFAULT_READ_CHILD_ENV: &str = "ROTTWEILER_SANDBOX_DEFAULT_READ_CHILD";
const WORKSPACE_READ_FIXTURE_ENV: &str = "ROTTWEILER_SANDBOX_WORKSPACE_READ_FIXTURE";
const TRUSTED_READ_FIXTURE_ENV: &str = "ROTTWEILER_SANDBOX_TRUSTED_READ_FIXTURE";

#[test]
fn sandboxed_default_read_policy_child() {
    if std::env::var_os(DEFAULT_READ_CHILD_ENV).is_none() {
        return;
    }
    let workspace_fixture =
        std::env::var_os(WORKSPACE_READ_FIXTURE_ENV).expect("workspace read fixture");
    assert_eq!(
        std::fs::read(workspace_fixture).expect("read workspace fixture"),
        b"workspace-readable"
    );
    assert!(
        std::fs::read("/etc/passwd")
            .expect("read reviewed system prefix")
            .starts_with(b"root:")
    );
    if let Some(trusted_fixture) = std::env::var_os(TRUSTED_READ_FIXTURE_ENV) {
        assert_eq!(
            std::fs::read(trusted_fixture).expect("read caller-trusted fixture"),
            b"trusted-readable"
        );
    }
    let home = std::env::var_os("HOME").expect("sandbox HOME");
    for suffix in [
        ".ssh/id_rsa",
        ".aws/credentials",
        ".kube/config",
        ".rottweiler/credentials",
    ] {
        let error = std::fs::read(Path::new(&home).join(suffix))
            .expect_err("sensitive home file was readable");
        assert!(
            error
                .raw_os_error()
                .is_some_and(|code| matches!(code, libc::EACCES | libc::EPERM)),
            "sensitive read failed for the wrong reason: {error}"
        );
    }
}

#[test]
fn sandboxed_exact_file_read_child() {
    let (Some(fixture), Some(output)) = (
        std::env::var_os(FILE_READ_FIXTURE_ENV),
        std::env::var_os(FILE_READ_OUTPUT_ENV),
    ) else {
        return;
    };
    let contents = std::fs::read(fixture).expect("read exact file root");
    assert_eq!(contents, b"exact-file-fixture");
    std::fs::write(output, b"exact-file-read").expect("write child result");
}

fn assert_socketpair_denied(
    family: nix::sys::socket::AddressFamily,
    kind: nix::sys::socket::SockType,
    protocol: Option<nix::sys::socket::SockProtocol>,
    flags: nix::sys::socket::SockFlag,
) {
    let error = nix::sys::socket::socketpair(family, kind, protocol, flags)
        .expect_err("socketpair shape unexpectedly passed seccomp");
    assert_eq!(
        error,
        nix::errno::Errno::EPERM,
        "socketpair shape did not reach the seccomp rail"
    );
}

#[test]
fn sandboxed_unix_datagram_pair_cannot_reach_a_bound_path() {
    let Some(target_path) = std::env::var_os(UNIX_DGRAM_TARGET_ENV) else {
        return;
    };
    let target_address = nix::sys::socket::UnixAddr::new(Path::new(&target_path))
        .expect("Unix datagram target address");
    match nix::sys::socket::socketpair(
        nix::sys::socket::AddressFamily::Unix,
        nix::sys::socket::SockType::Datagram,
        None,
        nix::sys::socket::SockFlag::SOCK_CLOEXEC,
    ) {
        Err(error) => assert_eq!(
            error,
            nix::errno::Errno::EPERM,
            "Unix datagram pair did not reach the seccomp socketpair rail"
        ),
        Ok((sender, _peer)) => {
            nix::sys::socket::connect(sender.as_raw_fd(), &target_address)
                .expect("vulnerable datagram pair connected to pathname socket");
            nix::sys::socket::send(
                sender.as_raw_fd(),
                b"sandbox-bypass",
                nix::sys::socket::MsgFlags::empty(),
            )
            .expect("vulnerable datagram pair sent to pathname socket");
            panic!("Unix datagram socketpair bypassed the policy-proxy boundary");
        }
    }
}

#[test]
fn sandboxed_tokio_unix_pair_child_completes_a_bounded_handshake() {
    if std::env::var_os(UNIX_PAIR_CHILD_ENV).is_none() {
        return;
    }

    for flags in [
        nix::sys::socket::SockFlag::empty(),
        nix::sys::socket::SockFlag::SOCK_CLOEXEC,
        nix::sys::socket::SockFlag::SOCK_NONBLOCK,
        nix::sys::socket::SockFlag::SOCK_CLOEXEC | nix::sys::socket::SockFlag::SOCK_NONBLOCK,
    ] {
        nix::sys::socket::socketpair(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            None,
            flags,
        )
        .expect("valid Unix stream pair flag combination");
    }
    for family in [
        nix::sys::socket::AddressFamily::Inet,
        nix::sys::socket::AddressFamily::Inet6,
    ] {
        assert_socketpair_denied(
            family,
            nix::sys::socket::SockType::Stream,
            None,
            nix::sys::socket::SockFlag::SOCK_CLOEXEC,
        );
    }
    for (kind, protocol, flags) in [
        (
            nix::sys::socket::SockType::Datagram,
            None,
            nix::sys::socket::SockFlag::SOCK_CLOEXEC,
        ),
        (
            nix::sys::socket::SockType::SeqPacket,
            None,
            nix::sys::socket::SockFlag::SOCK_NONBLOCK,
        ),
        (
            nix::sys::socket::SockType::Stream,
            Some(nix::sys::socket::SockProtocol::Tcp),
            nix::sys::socket::SockFlag::empty(),
        ),
        (
            nix::sys::socket::SockType::Stream,
            None,
            nix::sys::socket::SockFlag::from_bits_retain(1 << 29),
        ),
    ] {
        assert_socketpair_denied(nix::sys::socket::AddressFamily::Unix, kind, protocol, flags);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Tokio runtime");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (client, server) = tokio::net::UnixStream::pair().expect("Unix pair");
            client.writable().await.expect("client writable");
            assert_eq!(
                nix::unistd::write(&client, b"initialize").expect("client write"),
                10
            );
            server.readable().await.expect("server readable");
            let mut request = [0_u8; 10];
            assert_eq!(
                nix::unistd::read(server.as_raw_fd(), &mut request).expect("server read"),
                request.len()
            );
            assert_eq!(&request, b"initialize");
            server.writable().await.expect("server writable");
            assert_eq!(
                nix::unistd::write(&server, b"ready").expect("server write"),
                5
            );
            client.readable().await.expect("client readable");
            let mut response = [0_u8; 5];
            assert_eq!(
                nix::unistd::read(client.as_raw_fd(), &mut response).expect("client read"),
                response.len()
            );
            assert_eq!(&response, b"ready");
        })
        .await
        .expect("Tokio Unix-pair handshake timed out");
    });
}

#[test]
fn deny_and_proxy_modes_allow_only_unix_socketpairs_for_runtime_ipc() {
    if !sandbox_available(&rw_sandbox::probe(), "deny-mode Unix IPC")
        || !sandbox_available(&probe_policy_egress(), "policy-mode Unix IPC")
    {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let proxy =
        SupervisedEgressProxy::start(EgressPolicy::new(["example.com"])).expect("policy proxy");
    let relay_path = proxy.relay_path().expect("Linux relay path").to_path_buf();
    let policies = [
        SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny).expect("deny policy"),
        SandboxPolicy::new(
            [workspace.path()],
            NetworkPolicy::PolicyProxy {
                port: proxy.address().port(),
                relay_path: Some(relay_path),
            },
        )
        .expect("proxy policy"),
    ];

    for policy in policies {
        let args = [
            OsString::from("--exact"),
            OsString::from("sandboxed_tokio_unix_pair_child_completes_a_bounded_handshake"),
            OsString::from("--nocapture"),
        ];
        let mut command = test_helper_command(
            &policy,
            &std::env::current_exe().expect("current test executable"),
            &args,
        );
        let status = command
            .env(UNIX_PAIR_CHILD_ENV, "1")
            .status()
            .expect("sandboxed Tokio Unix-pair child");
        assert!(
            status.success(),
            "sandboxed Unix-pair child exited {status}"
        );
    }
}

#[test]
fn policy_proxy_blocks_unix_datagram_pair_path_bypass() {
    if !sandbox_available(
        &probe_policy_egress(),
        "policy-mode Unix datagram isolation",
    ) {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let target_path = workspace.path().join("datagram-target.sock");
    let target = UnixDatagram::bind(&target_path).expect("bound Unix datagram target");
    target.set_nonblocking(true).expect("nonblocking target");
    let proxy =
        SupervisedEgressProxy::start(EgressPolicy::new(["example.com"])).expect("policy proxy");
    let policy = SandboxPolicy::new(
        [workspace.path()],
        NetworkPolicy::PolicyProxy {
            port: proxy.address().port(),
            relay_path: Some(proxy.relay_path().expect("Linux relay path").to_path_buf()),
        },
    )
    .expect("proxy policy");
    let args = [
        OsString::from("--exact"),
        OsString::from("sandboxed_unix_datagram_pair_cannot_reach_a_bound_path"),
        OsString::from("--nocapture"),
    ];
    let mut command = test_helper_command(
        &policy,
        &std::env::current_exe().expect("current test executable"),
        &args,
    );
    let status = command
        .env(UNIX_DGRAM_TARGET_ENV, &target_path)
        .status()
        .expect("sandboxed Unix datagram canary");
    assert!(status.success(), "Unix datagram canary exited {status}");

    let mut message = [0_u8; 32];
    let error = target
        .recv(&mut message)
        .expect_err("sandboxed child reached the bound Unix datagram target");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}

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
fn regular_file_read_roots_allow_exact_data_and_executable_files() {
    if !sandbox_available(&rw_sandbox::probe(), "regular-file read roots") {
        return;
    }
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let fixture = fixtures.path().join("fixture.txt");
    std::fs::write(&fixture, b"exact-file-fixture").expect("fixture");
    let output = workspace.path().join("read-result");
    let executable = std::env::current_exe().expect("current test executable");
    let policy = SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny)
        .and_then(|policy| {
            policy.with_read_roots([
                executable.as_path(),
                fixture.as_path(),
                Path::new("/usr/lib"),
                Path::new("/etc/ld.so.cache"),
            ])
        })
        .expect("file-root policy");
    let args = [
        OsString::from("--exact"),
        OsString::from("sandboxed_exact_file_read_child"),
        OsString::from("--nocapture"),
    ];
    let mut command = test_helper_command(&policy, &executable, &args);
    let status = command
        .env(FILE_READ_FIXTURE_ENV, &fixture)
        .env(FILE_READ_OUTPUT_ENV, &output)
        .status()
        .expect("sandboxed exact-file child");
    assert!(status.success(), "exact-file child exited {status}");
    assert_eq!(
        std::fs::read(output).expect("child result"),
        b"exact-file-read"
    );
}

#[test]
fn default_read_policy_excludes_home_secrets_but_keeps_required_roots() {
    if !sandbox_available(&rw_sandbox::probe(), "default read-root isolation") {
        return;
    }
    let host_home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
        .expect("Linux security tests require a writable HOME");
    let sandbox_home = tempfile::Builder::new()
        .prefix("rottweiler-sandbox-home-")
        .tempdir_in(host_home)
        .expect("sandbox HOME");
    for suffix in [
        ".ssh/id_rsa",
        ".aws/credentials",
        ".kube/config",
        ".rottweiler/credentials",
    ] {
        let path = sandbox_home.path().join(suffix);
        std::fs::create_dir_all(path.parent().expect("secret parent")).expect("secret directory");
        std::fs::write(path, b"credential-canary").expect("secret fixture");
    }
    let trusted = sandbox_home.path().join(".cache/toolchain/trusted");
    std::fs::create_dir_all(trusted.parent().expect("trusted parent")).expect("trusted directory");
    std::fs::write(&trusted, b"trusted-readable").expect("trusted fixture");
    let benign_home_file = sandbox_home.path().join("project.txt");
    std::fs::write(&benign_home_file, b"workspace-readable").expect("home workspace fixture");

    let workspace = tempdir().expect("workspace");
    let workspace_fixture = workspace.path().join("workspace.txt");
    std::fs::write(&workspace_fixture, b"workspace-readable").expect("workspace fixture");
    let executable = std::env::current_exe().expect("current test executable");
    let args = [
        OsString::from("--exact"),
        OsString::from("sandboxed_default_read_policy_child"),
        OsString::from("--nocapture"),
    ];

    let default_policy =
        SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny).expect("default policy");
    assert_read_policy_case(
        &default_policy,
        &executable,
        &args,
        sandbox_home.path(),
        &workspace_fixture,
        None,
    );

    let trusted_policy = SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny)
        .and_then(|policy| policy.with_read_roots([executable.as_path(), trusted.as_path()]))
        .expect("trusted-root policy");
    assert_read_policy_case(
        &trusted_policy,
        &executable,
        &args,
        sandbox_home.path(),
        &workspace_fixture,
        Some(&trusted),
    );

    // Landlock cannot subtract from a parent grant. The implementation keeps
    // a HOME workspace readable by granting its existing non-sensitive sibling
    // entries, while the credential directories remain absent from the ruleset.
    let home_workspace_policy =
        SandboxPolicy::new([sandbox_home.path()], NetworkPolicy::Deny).expect("HOME policy");
    assert_read_policy_case(
        &home_workspace_policy,
        &executable,
        &args,
        sandbox_home.path(),
        &benign_home_file,
        Some(&trusted),
    );
}

fn assert_read_policy_case(
    policy: &SandboxPolicy,
    executable: &Path,
    args: &[OsString],
    home: &Path,
    workspace_fixture: &Path,
    trusted_fixture: Option<&Path>,
) {
    let mut command = test_helper_command(policy, executable, args);
    command
        .env(DEFAULT_READ_CHILD_ENV, "1")
        .env("HOME", home)
        .env(WORKSPACE_READ_FIXTURE_ENV, workspace_fixture);
    if let Some(trusted_fixture) = trusted_fixture {
        command.env(TRUSTED_READ_FIXTURE_ENV, trusted_fixture);
    }
    let status = command.status().expect("sandboxed read-policy child");
    assert!(status.success(), "read-policy child exited {status}");
}

#[test]
fn regular_file_write_root_allows_only_that_file() {
    if !sandbox_available(&rw_sandbox::probe(), "regular-file write roots") {
        return;
    }
    let container = tempdir().expect("container");
    let writable = container.path().join("approved.txt");
    std::fs::write(&writable, b"before").expect("writable fixture");
    let policy = SandboxPolicy::new([&writable], NetworkPolicy::Deny).expect("file-write policy");
    let args = [
        OsString::from("-c"),
        OsString::from("printf changed > \"$1\""),
        OsString::from("file-write-canary"),
        writable.as_os_str().to_owned(),
    ];
    let status = test_helper_command(&policy, Path::new("/bin/sh"), &args)
        .status()
        .expect("regular-file write command");
    assert!(status.success(), "file-write command exited {status}");
    assert_eq!(std::fs::read(writable).expect("write result"), b"changed");
}

#[test]
fn root_type_swaps_after_policy_creation_fail_closed() {
    if !sandbox_available(&rw_sandbox::probe(), "root type-swap isolation") {
        return;
    }

    let file_case = tempdir().expect("file case");
    let file_root = file_case.path().join("root");
    let old_file = file_case.path().join("old-file");
    std::fs::write(&file_root, b"file").expect("file root");
    let file_policy = SandboxPolicy::new([&file_root], NetworkPolicy::Deny).expect("file policy");
    std::fs::rename(&file_root, old_file).expect("move file root");
    std::fs::create_dir(&file_root).expect("replacement directory");
    let status = test_helper_command(&file_policy, Path::new("/bin/true"), &[])
        .status()
        .expect("file-to-directory swap command");
    assert!(!status.success(), "file-to-directory swap was accepted");

    let directory_case = tempdir().expect("directory case");
    let directory_root = directory_case.path().join("root");
    let old_directory = directory_case.path().join("old-directory");
    std::fs::create_dir(&directory_root).expect("directory root");
    let directory_policy =
        SandboxPolicy::new([&directory_root], NetworkPolicy::Deny).expect("directory policy");
    std::fs::rename(&directory_root, old_directory).expect("move directory root");
    std::fs::write(&directory_root, b"replacement file").expect("replacement file");
    let status = test_helper_command(&directory_policy, Path::new("/bin/true"), &[])
        .status()
        .expect("directory-to-file swap command");
    assert!(!status.success(), "directory-to-file swap was accepted");
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
