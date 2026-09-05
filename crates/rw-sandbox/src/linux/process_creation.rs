//! A proxy relay may spawn its trusted bootstrap; untrusted workers may only create threads.
use super::{
    close_helper_pin, command_without_helper_pin, inherited_helper_pin, install_landlock,
    install_network_floor, sandbox_backend,
};
use crate::{NetworkPolicy, SandboxError, SandboxPolicy};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::process::CommandExt as _,
    process::{Command, ExitStatus},
};

pub(crate) const WORKER_ARG: &str = "--rottweiler-sandbox-process-worker";

pub(super) fn restrict_if_requested(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    if policy.allow_process_creation {
        return Ok(());
    }
    // clone3's indirect argument cannot be inspected by classic BPF. ENOSYS
    // makes libc use clone, whose CLONE_THREAD bit the second filter checks.
    apply(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        libc::ENOSYS,
    )?;
    let no_thread = SeccompRule::new(vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::MaskedEq(libc::CLONE_THREAD as u64),
            0,
        )
        .map_err(sandbox_backend)?,
    ])
    .map_err(sandbox_backend)?;
    let denied = BTreeMap::from([(libc::SYS_clone, vec![no_thread])]);
    #[cfg(target_arch = "x86_64")]
    let denied = {
        let mut denied = denied;
        denied.insert(libc::SYS_fork, Vec::new());
        denied.insert(libc::SYS_vfork, Vec::new());
        denied
    };
    apply(denied, libc::EPERM)
}

fn apply(denied: BTreeMap<i64, Vec<SeccompRule>>, errno: i32) -> Result<(), SandboxError> {
    let filter: BpfProgram = SeccompFilter::new(
        denied,
        SeccompAction::Allow,
        SeccompAction::Errno(u32::try_from(errno).map_err(sandbox_backend)?),
        std::env::consts::ARCH.try_into().map_err(sandbox_backend)?,
    )
    .map_err(sandbox_backend)?
    .try_into()
    .map_err(sandbox_backend)?;
    seccompiler::apply_filter(&filter).map_err(sandbox_backend)
}

pub(crate) fn run_worker(args: &[OsString]) -> Result<std::convert::Infallible, SandboxError> {
    if args.len() < 4 {
        return Err(SandboxError::MalformedHelper);
    }
    let policy: SandboxPolicy = crate::serde_json::from_os_str(&args[2])?;
    if policy.allow_process_creation
        || policy.preparation.is_some()
        || !matches!(policy.network, NetworkPolicy::PolicyProxy { .. })
    {
        return Err(SandboxError::MalformedHelper);
    }
    let helper_pin = inherited_helper_pin(args)?;
    install_landlock(&policy, &args[3])?;
    install_network_floor(true)?;
    restrict_if_requested(&policy)?;
    Err(SandboxError::Exec(
        command_without_helper_pin(&args[3], &args[4..], helper_pin)?.exec(),
    ))
}

pub(super) fn run_proxy_worker(
    policy: &SandboxPolicy,
    program: &OsString,
    args: &[OsString],
    helper_pin: Option<u32>,
) -> Result<ExitStatus, SandboxError> {
    let helper = match helper_pin {
        Some(fd) => std::path::PathBuf::from(format!("/proc/self/fd/{fd}")),
        None => std::env::current_exe().map_err(SandboxError::Exec)?,
    };
    let mut child = Command::new(helper)
        .arg(WORKER_ARG)
        .arg(crate::serde_json::to_os_string(policy)?)
        .arg(program)
        .args(args)
        .spawn()
        .map_err(SandboxError::Exec)?;
    // The trusted relay and child install their own floors. The child never
    // executes the target before its complete filesystem/network/process policy.
    let parent_policy = close_helper_pin(helper_pin)
        .and_then(|()| install_landlock(policy, program))
        .and_then(|()| install_network_floor(true));
    if let Err(error) = parent_policy {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    child.wait().map_err(SandboxError::Exec)
}
