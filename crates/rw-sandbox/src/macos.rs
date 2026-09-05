//! Seatbelt policy and trusted worker launch shape.
use super::{
    LaunchPlan, NetworkPolicy, SandboxError, SandboxPolicy, directory_reads, proxy,
    sensitive_home_roots,
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
pub(super) const WORKER_ARG: &str = "--rw-macos-worker";
pub(super) fn launch_plan(
    policy: &SandboxPolicy,
    helper_executable: &Path,
    shell: &Path,
    shell_args: &[OsString],
) -> Result<LaunchPlan, SandboxError> {
    if let NetworkPolicy::PolicyProxy { port, .. } = &policy.network
        && !proxy::supervised_proxy_owns_port(*port)
    {
        return Err(SandboxError::PolicyProxyUnavailable);
    }
    let mut args = vec![
        OsString::from("-p"),
        OsString::from(seatbelt_profile(policy)),
    ];
    for (index, root) in policy.write_roots.iter().enumerate() {
        args.push(OsString::from("-D"));
        let mut definition = OsString::from(format!("RW_WRITE_{index}="));
        definition.push(root.as_os_str());
        args.push(definition);
    }
    if let Some(read_roots) = &policy.read_roots {
        for (index, root) in read_roots.iter().enumerate() {
            args.push(OsString::from("-D"));
            let mut definition = OsString::from(format!("RW_READ_{index}="));
            definition.push(root.as_os_str());
            args.push(definition);
        }
    }
    directory_reads::append_parameters(policy, &mut args);
    for (index, root) in sensitive_read_roots().iter().enumerate() {
        args.push(OsString::from("-D"));
        let mut definition = OsString::from(format!("RW_SECRET_{index}="));
        definition.push(root.as_os_str());
        args.push(definition);
    }
    if !policy.allow_process_creation {
        let helper = std::fs::canonicalize(helper_executable).map_err(|error| {
            SandboxError::Unavailable(format!("invalid macOS worker helper: {error}"))
        })?;
        args.push(OsString::from("-D"));
        let mut definition = OsString::from("RW_WORKER_HELPER=");
        definition.push(helper.as_os_str());
        args.push(definition);
        args.push(helper.into_os_string());
        args.push(OsString::from(WORKER_ARG));
    }
    args.push(shell.as_os_str().to_owned());
    args.extend_from_slice(shell_args);
    Ok(LaunchPlan {
        program: PathBuf::from("/usr/bin/sandbox-exec"),
        args,
        warnings: Vec::new(),
    })
}

fn seatbelt_profile(policy: &SandboxPolicy) -> String {
    let authority = if policy.allow_process_creation {
        "(allow default)"
    } else {
        "(deny default) (allow file-read* file-write* file-map-executable sysctl-read process-exec) (allow process-info* signal (target self))"
    };
    let writable = (0..policy.write_roots.len())
        .map(|index| format!("(subpath (param \"RW_WRITE_{index}\"))"))
        .collect::<Vec<_>>()
        .join(" ");
    let readable = policy.read_roots.as_ref().map(|roots| {
        (0..roots.len())
            .map(|index| format!("(subpath (param \"RW_READ_{index}\"))"))
            .collect::<Vec<_>>()
            .join(" ")
    });
    let network = match &policy.network {
        NetworkPolicy::Deny => "(deny network*)".to_owned(),
        NetworkPolicy::PolicyProxy { port, .. } => format!(
            "(allow network-outbound (remote ip \"localhost:{port}\")) (deny network-outbound (require-not (remote ip \"localhost:{port}\"))) (deny network-bind) (deny network-inbound)"
        ),
    };
    let directory_entries = (0..policy.read_directory_ancestors.len())
        .map(|index| {
            format!(
                "(require-all (literal (param \"RW_DIRECTORY_{index}\")) (vnode-type DIRECTORY))"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let helper_read = if policy.allow_process_creation {
        ""
    } else {
        "(literal (param \"RW_WORKER_HELPER\"))"
    };
    let read_rule = readable.map_or_else(String::new, |readable| {
        format!(
            "(deny file-read* (require-not (require-any (literal \"/\") (literal \"/dev/null\") {writable} {readable} {directory_entries} {helper_read})))"
        )
    });
    let secret_roots = (0..sensitive_read_roots().len())
        .map(|index| format!("(subpath (param \"RW_SECRET_{index}\"))"))
        .collect::<Vec<_>>()
        .join(" ");
    let secret_rule = if secret_roots.is_empty() {
        String::new()
    } else {
        format!("(deny file-read* (require-any {secret_roots}))")
    };
    format!(
        "(version 1) {authority} {read_rule} {secret_rule} (deny file-write* (require-not (require-any (literal \"/dev/null\") {writable}))) {network}"
    )
}

pub(super) fn sensitive_read_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    else {
        return Vec::new();
    };
    sensitive_home_roots(&home)
}
