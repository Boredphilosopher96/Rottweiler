mod authority;
mod loopback;
mod preparation;
pub(super) mod process_creation;
#[cfg(test)]
mod root_grants_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto as _;
use std::io;
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

use landlock::{
    ABI, Access, AccessFs, PathBeneath, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};

use super::{
    NetworkPolicy, OsString, RootKind, SandboxError, SandboxPolicy, sensitive_home_roots,
    serde_json,
};

/// Linux's default shell runtime roots. These are deliberately explicit:
/// Landlock cannot subtract a credential directory after granting `/`.
/// Optional multilib and virtual-filesystem entries are skipped when the
/// host does not provide them.
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc",
    "/proc/self",
    "/sys/devices/system/cpu",
    "/sys/fs/cgroup",
    "/tmp",
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];

pub(super) fn run_helper(args: &[OsString]) -> Result<std::convert::Infallible, SandboxError> {
    if args.len() < 4 {
        return Err(SandboxError::MalformedHelper);
    }
    let policy: SandboxPolicy = serde_json::from_os_str(&args[2])?;
    let helper_pin = inherited_helper_pin(args)?;
    if let Some(layout) = &policy.preparation {
        return preparation::run(&policy, layout, &args[3], &args[4..], helper_pin);
    }
    if let NetworkPolicy::PolicyProxy {
        port,
        relay_path: Some(relay_path),
    } = &policy.network
    {
        return run_proxy_helper(&policy, *port, relay_path, &args[3], &args[4..], helper_pin);
    }
    if policy.network != NetworkPolicy::Deny {
        return Err(SandboxError::MalformedHelper);
    }
    install_landlock(&policy, &args[3])?;
    install_network_floor(false)?;
    process_creation::restrict_if_requested(&policy)?;
    let error = command_without_helper_pin(&args[3], &args[4..], helper_pin)?.exec();
    Err(SandboxError::Exec(error))
}

fn inherited_helper_pin(args: &[OsString]) -> Result<Option<u32>, SandboxError> {
    let Some(executable) = args.first().and_then(|argument| argument.to_str()) else {
        return Err(SandboxError::MalformedHelper);
    };
    let Some(descriptor) = executable.strip_prefix("/proc/self/fd/") else {
        return Ok(None);
    };
    descriptor
        .parse::<u32>()
        .ok()
        .filter(|descriptor| *descriptor >= 3)
        .map(Some)
        .ok_or(SandboxError::MalformedHelper)
}

fn command_without_helper_pin(
    program: &OsString,
    args: &[OsString],
    helper_pin: Option<u32>,
) -> Result<Command, SandboxError> {
    close_helper_pin(helper_pin)?;
    let mut command = Command::new(program);
    command.args(args);
    Ok(command)
}

fn close_helper_pin(helper_pin: Option<u32>) -> Result<(), SandboxError> {
    if let Some(helper_pin) = helper_pin {
        struct InheritedHelperPin(i32);

        impl std::os::fd::IntoRawFd for InheritedHelperPin {
            fn into_raw_fd(self) -> i32 {
                self.0
            }
        }

        let helper_pin: i32 = helper_pin
            .try_into()
            .map_err(|_| SandboxError::MalformedHelper)?;
        // The descriptor was validated from /proc/self/fd and was needed
        // only to pin the helper across the unshare exec. Transfer its
        // ownership to nix 0.31 and close it before launching the target.
        nix::unistd::close(InheritedHelperPin(helper_pin)).map_err(sandbox_backend)?;
    }
    Ok(())
}

fn run_proxy_helper(
    policy: &SandboxPolicy,
    port: u16,
    relay_path: &Path,
    program: &OsString,
    args: &[OsString],
    helper_pin: Option<u32>,
) -> Result<std::convert::Infallible, SandboxError> {
    if port == 0 || !relay_path.is_absolute() {
        return Err(SandboxError::MalformedHelper);
    }
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(sandbox_backend)?;
    loopback::raise()?;
    authority::lock_setup_authority()?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(SandboxError::Proxy)?;
    listener
        .set_nonblocking(true)
        .map_err(SandboxError::Proxy)?;
    let running = Arc::new(AtomicBool::new(true));
    let relay_running = Arc::clone(&running);
    let relay_path = relay_path.to_path_buf();
    let relay = thread::Builder::new()
        .name("rottweiler-egress-netns-relay".to_owned())
        .spawn(move || serve_namespace_relay(&listener, &relay_path, &relay_running))
        .map_err(SandboxError::Proxy)?;

    let status = if policy.allow_process_creation {
        install_landlock(policy, program)?;
        install_network_floor(true)?;
        command_without_helper_pin(program, args, helper_pin)?
            .status()
            .map_err(SandboxError::Exec)?
    } else {
        process_creation::run_proxy_worker(policy, program, args, helper_pin)?
    };
    running.store(false, Ordering::Release);
    let _ = TcpStream::connect_timeout(
        &(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(100),
    );
    let _ = relay.join();
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    std::process::exit(128 + status.signal().unwrap_or(1));
}

fn serve_namespace_relay(listener: &TcpListener, relay_path: &Path, running: &Arc<AtomicBool>) {
    let active = Arc::new(AtomicUsize::new(0));
    while running.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((client, _)) => {
                if active
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        (count < 64).then_some(count + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let path = relay_path.to_path_buf();
                let active = Arc::clone(&active);
                let _ = thread::Builder::new()
                    .name("rottweiler-egress-netns-connection".to_owned())
                    .spawn(move || {
                        if let Ok(upstream) = UnixStream::connect(path) {
                            let _ = tunnel_tcp_to_unix(client, upstream);
                        }
                        active.fetch_sub(1, Ordering::AcqRel);
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

fn tunnel_tcp_to_unix(mut client: TcpStream, mut upstream: UnixStream) -> io::Result<()> {
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let forward = thread::spawn(move || {
        let result = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
        result
    });
    let reverse = io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = forward.join();
    reverse.map(|_| ())
}

fn install_landlock(policy: &SandboxPolicy, program: &OsString) -> Result<(), SandboxError> {
    // V3 includes REFER and TRUNCATE.  Requiring full enforcement prevents
    // older kernels from silently leaving path-based truncate unrestricted.
    let abi = ABI::V3;
    let all = AccessFs::from_all(abi);
    let read = AccessFs::from_read(abi);
    let mut ruleset = Ruleset::default()
        .handle_access(all)
        .map_err(sandbox_backend)?
        .create()
        .map_err(sandbox_backend)?;
    let homes = linux_homes();
    let sensitive = homes
        .iter()
        .flat_map(|home| sensitive_linux_roots(home))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut read_grants = BTreeMap::new();
    if policy.system_read_roots {
        for root in SYSTEM_READ_ROOTS {
            collect_system_read_root(Path::new(root), &homes, &mut read_grants)?;
        }
    }
    if policy.self_process_reads {
        collect_system_read_root(Path::new("/proc/self"), &homes, &mut read_grants)?;
    }
    match (&policy.read_roots, &policy.read_root_kinds) {
        (Some(read_roots), Some(read_root_kinds)) if read_roots.len() == read_root_kinds.len() => {
            for (root, kind) in read_roots.iter().zip(read_root_kinds) {
                collect_authorized_read_root(root, *kind, &sensitive, &mut read_grants)?;
            }
        }
        (None, None) => {
            if let Some(program) = absolute_existing_root(Path::new(program))? {
                collect_authorized_read_root(&program.0, program.1, &sensitive, &mut read_grants)?;
            }
        }
        _ => return Err(SandboxError::MalformedHelper),
    }
    if policy.write_roots.len() != policy.write_root_kinds.len() {
        return Err(SandboxError::MalformedHelper);
    }
    for (root, kind) in policy.write_roots.iter().zip(&policy.write_root_kinds) {
        collect_authorized_read_root(root, *kind, &sensitive, &mut read_grants)?;
    }
    for (root, kind) in read_grants {
        let root = open_landlock_root(&root, kind)?;
        let access = if kind == RootKind::Directory {
            read
        } else {
            read & AccessFs::from_file(abi)
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(root, access))
            .map_err(sandbox_backend)?;
    }
    for (root_path, kind) in policy.write_roots.iter().zip(&policy.write_root_kinds) {
        // A write root containing a sensitive home path cannot receive an
        // `all` rule: Landlock rules are additive, so that would restore
        // READ_FILE below the excluded path. Grant write authority at the
        // parent and add read authority only on the safe sibling snapshot.
        let root_contains_sensitive = sensitive
            .iter()
            .any(|secret| secret.starts_with(root_path) || root_path.starts_with(secret));
        let write_access = if root_contains_sensitive {
            all & !read
        } else {
            all
        };
        let root = open_landlock_root(root_path, *kind)?;
        let access = if *kind == RootKind::Directory {
            write_access
        } else {
            write_access & AccessFs::from_file(abi)
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(root, access))
            .map_err(sandbox_backend)?;
    }
    let status = ruleset.restrict_self().map_err(sandbox_backend)?;
    if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
        return Err(SandboxError::Unavailable(format!(
            "Landlock V3 is not fully enforced ({:?}); refusing unsandboxed execution",
            status.ruleset
        )));
    }
    Ok(())
}

pub(super) fn linux_homes() -> Vec<PathBuf> {
    let mut homes = BTreeSet::new();
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .and_then(|path| path.canonicalize().ok())
        .filter(|path| path.is_dir())
    {
        homes.insert(home);
    }
    // HOME is caller-controlled process state. Also consult the local
    // account database so overriding HOME cannot expose the real account's
    // credential stores. Hosts using a remote identity database still get
    // the environment-derived path and the explicit allowlist floor.
    let uid = rustix::process::getuid().as_raw().to_string();
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for fields in passwd
            .lines()
            .map(|line| line.split(':').collect::<Vec<_>>())
        {
            if fields.get(2) == Some(&uid.as_str())
                && let Some(home) = fields
                    .get(5)
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .and_then(|path| path.canonicalize().ok())
                    .filter(|path| path.is_dir())
            {
                homes.insert(home);
            }
        }
    }
    homes.into_iter().collect()
}

pub(super) fn sensitive_linux_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for lexical in sensitive_home_roots(home) {
        roots.insert(lexical.clone());
        if let Ok(canonical) = lexical.canonicalize() {
            roots.insert(canonical);
        }
    }
    roots.into_iter().collect()
}

fn collect_system_read_root(
    root: &Path,
    homes: &[PathBuf],
    grants: &mut BTreeMap<PathBuf, RootKind>,
) -> Result<(), SandboxError> {
    let Some((root, kind)) = absolute_existing_root(root)? else {
        return Ok(());
    };
    if homes.iter().any(|home| root.starts_with(home)) {
        return Ok(());
    }
    let excluded = homes
        .iter()
        .filter(|home| home.starts_with(&root))
        .cloned()
        .collect::<Vec<_>>();
    if kind == RootKind::Directory && !excluded.is_empty() {
        return collect_directory_except(&root, &excluded, grants);
    }
    grants.insert(root, kind);
    Ok(())
}

/// Adds an explicitly authorized root while carving credential paths out
/// of any parent grant. Landlock has no deny rule, so a home-directory
/// workspace is represented by rules for its existing safe siblings. This
/// preserves reads of existing workspace content, but the home directory
/// itself cannot be listed and new top-level entries are not readable until
/// a future sandbox invocation rebuilds the snapshot.
fn collect_authorized_read_root(
    root: &Path,
    kind: RootKind,
    sensitive: &[PathBuf],
    grants: &mut BTreeMap<PathBuf, RootKind>,
) -> Result<(), SandboxError> {
    if sensitive.iter().any(|secret| root.starts_with(secret)) {
        return Ok(());
    }
    let excluded = sensitive
        .iter()
        .filter(|secret| secret.starts_with(root))
        .cloned()
        .collect::<Vec<_>>();
    if kind == RootKind::Directory && !excluded.is_empty() {
        collect_directory_except(root, &excluded, grants)
    } else {
        grants.insert(root.to_path_buf(), kind);
        Ok(())
    }
}

fn collect_directory_except(
    root: &Path,
    excluded: &[PathBuf],
    grants: &mut BTreeMap<PathBuf, RootKind>,
) -> Result<(), SandboxError> {
    for entry in std::fs::read_dir(root).map_err(sandbox_backend)? {
        let entry = entry.map_err(sandbox_backend)?;
        let path = entry.path();
        let link_metadata = path.symlink_metadata().map_err(sandbox_backend)?;
        // A PathBeneath rule is attached to the resolved inode. Following
        // a sibling symlink here could accidentally grant an object outside
        // the authorized root, so snapshot carving deliberately omits it.
        if link_metadata.file_type().is_symlink() {
            continue;
        }
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(sandbox_backend(error)),
        };
        if !canonical.starts_with(root) {
            continue;
        }
        let is_excluded = excluded
            .iter()
            .any(|secret| path == *secret || canonical == *secret);
        if is_excluded {
            continue;
        }
        let nested = excluded
            .iter()
            .filter(|secret| secret.starts_with(&path) || secret.starts_with(&canonical))
            .cloned()
            .collect::<Vec<_>>();
        let metadata = canonical.metadata().map_err(sandbox_backend)?;
        let kind = RootKind::for_metadata(&metadata);
        if kind == RootKind::Directory && !nested.is_empty() {
            collect_directory_except(&path, &nested, grants)?;
        } else {
            grants.insert(canonical, kind);
        }
    }
    Ok(())
}

fn absolute_existing_root(root: &Path) -> Result<Option<(PathBuf, RootKind)>, SandboxError> {
    if !root.is_absolute() {
        return Ok(None);
    }
    let canonical = match root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(sandbox_backend(error)),
    };
    let metadata = canonical.metadata().map_err(sandbox_backend)?;
    Ok(Some((canonical, RootKind::for_metadata(&metadata))))
}

fn open_landlock_root(root: &Path, expected: RootKind) -> Result<OwnedFd, SandboxError> {
    // Policy roots are canonical paths. Refuse a substituted symlink in any
    // component, including a same-kind replacement that a type check cannot
    // detect. The descriptor used for classification is the actual rule parent.
    let root = rustix::fs::openat2(
        rustix::fs::CWD,
        root,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS,
    )
    .map_err(sandbox_backend)?;
    let metadata = rustix::fs::fstat(root.as_fd()).map_err(sandbox_backend)?;
    let actual = if rustix::fs::FileType::from_raw_mode(metadata.st_mode).is_dir() {
        RootKind::Directory
    } else {
        RootKind::NonDirectory
    };
    if actual != expected {
        return Err(SandboxError::RootTypeChanged);
    }
    Ok(root)
}

fn install_network_floor(policy_proxy: bool) -> Result<(), SandboxError> {
    let mut denied = [
        libc::SYS_bind,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ]
    .into_iter()
    .map(|syscall| (syscall, Vec::new()))
    .collect::<BTreeMap<_, _>>();
    denied.insert(libc::SYS_socketpair, local_connected_pair_rules()?);
    if policy_proxy {
        let non_inet = SeccompRule::new(vec![
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                libc::AF_INET as u64,
            )
            .map_err(sandbox_backend)?,
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                libc::AF_INET6 as u64,
            )
            .map_err(sandbox_backend)?,
        ])
        .map_err(sandbox_backend)?;
        denied.insert(libc::SYS_socket, vec![non_inet]);
    } else {
        // Deny socket creation outright. The empty network namespace is
        // the authority boundary; these syscall rails additionally cover
        // inherited descriptors and UDP/raw/async submission paths.
        denied.insert(libc::SYS_socket, Vec::new());
        denied.insert(libc::SYS_connect, Vec::new());
        for syscall in [
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_sendmmsg,
            libc::SYS_recvmsg,
            libc::SYS_recvmmsg,
        ] {
            denied.insert(syscall, Vec::new());
        }
    }
    let filter: BpfProgram = SeccompFilter::new(
        denied,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH.try_into().map_err(sandbox_backend)?,
    )
    .map_err(sandbox_backend)?
    .try_into()
    .map_err(sandbox_backend)?;
    seccompiler::apply_filter(&filter).map_err(sandbox_backend)
}

fn local_connected_pair_rules() -> Result<Vec<SeccompRule>, SandboxError> {
    // Connected stream/sequence-packet pairs are local IPC used by Tokio
    // and Rust's fork/exec error channel. Neither kind can be retargeted.
    // Datagram pairs are deliberately excluded because one endpoint can
    // be reconnected to a pathname socket after creation. Exact type
    // matching also rejects every flag except CLOEXEC and NONBLOCK.
    let non_unix_pair = SeccompRule::new(vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )
        .map_err(sandbox_backend)?,
    ])
    .map_err(sandbox_backend)?;
    let mut invalid_unix_pair_type = vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_UNIX as u64,
        )
        .map_err(sandbox_backend)?,
    ];
    for allowed_type in [
        libc::SOCK_STREAM,
        libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
        libc::SOCK_STREAM | libc::SOCK_NONBLOCK,
        libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        libc::SOCK_SEQPACKET,
        libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
        libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK,
        libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
    ] {
        invalid_unix_pair_type.push(
            SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                u64::try_from(allowed_type).map_err(sandbox_backend)?,
            )
            .map_err(sandbox_backend)?,
        );
    }
    let invalid_unix_pair_type =
        SeccompRule::new(invalid_unix_pair_type).map_err(sandbox_backend)?;
    let nonzero_pair_protocol = SeccompRule::new(vec![
        SeccompCondition::new(2, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, 0)
            .map_err(sandbox_backend)?,
    ])
    .map_err(sandbox_backend)?;
    Ok(vec![
        non_unix_pair,
        invalid_unix_pair_type,
        nonzero_pair_protocol,
    ])
}

fn sandbox_backend(error: impl std::fmt::Display) -> SandboxError {
    SandboxError::Backend(error.to_string())
}
