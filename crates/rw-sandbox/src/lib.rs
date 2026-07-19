//! OS-native sandbox policy, capability probing, and launch-plan construction.
//!
//! The crate deliberately does not execute commands.  It turns a reviewed
//! policy into an argv-only launch plan consumed by `rw-tools`, and exposes the
//! Linux helper entry point used immediately before `exec(2)`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod proxy;
pub use proxy::{EgressPin, ProxyDenials, ProxyLifecycle, SupervisedEgressProxy, UpstreamProxy};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "sandbox";

/// The internal argv marker handled before the public CLI parser starts.
pub const HELPER_ARG: &str = "__rottweiler-sandbox-helper";

// Keep this list shared by Seatbelt and Landlock so broad/default policies do
// not drift in which credential stores they protect.
const SENSITIVE_HOME_SUFFIXES: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".codex",
    ".docker",
    ".gnupg",
    ".kube",
    ".rottweiler",
    ".config/gcloud",
    ".config/gh",
    ".config/opencode",
    ".git-credentials",
    ".netrc",
    ".npmrc",
    ".pypirc",
];

/// Network authority granted to a sandboxed process.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// All socket connection and bind attempts are denied.
    #[default]
    Deny,
    /// Egress is possible only through a separately supervised policy proxy.
    ///
    /// Linux routes the isolated namespace through a private relay; macOS
    /// grants only the supervisor's exact loopback port. Missing relay state
    /// fails closed instead of granting ambient network access.
    PolicyProxy {
        /// Loopback port of the supervised host-side proxy.
        port: u16,
        /// Private pathname socket used by the Linux network-namespace relay.
        #[serde(default)]
        relay_path: Option<PathBuf>,
    },
}

/// Per-invocation filesystem and network authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    write_roots: Vec<PathBuf>,
    write_root_kinds: Vec<RootKind>,
    #[serde(default)]
    read_roots: Option<Vec<PathBuf>>,
    #[serde(default)]
    read_root_kinds: Option<Vec<RootKind>>,
    network: NetworkPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RootKind {
    Directory,
    NonDirectory,
}

impl RootKind {
    fn for_metadata(metadata: &std::fs::Metadata) -> Self {
        if metadata.is_dir() {
            Self::Directory
        } else {
            Self::NonDirectory
        }
    }
}

impl SandboxPolicy {
    /// Creates a policy after resolving every writable root to an existing,
    /// absolute filesystem object.
    ///
    /// Linux's default read policy grants only the writable roots, a reviewed
    /// set of system/runtime roots, and the executable selected for launch. It
    /// does not grant the user's home directory. macOS retains its broad-read
    /// compatibility policy with explicit credential-root denials.
    ///
    /// # Errors
    ///
    /// Returns an error when no root is supplied, a root does not exist, or a
    /// root cannot be canonicalized.
    pub fn new<I, P>(write_roots: I, network: NetworkPolicy) -> Result<Self, SandboxError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut roots = BTreeMap::new();
        for root in write_roots {
            let supplied = root.as_ref();
            let canonical =
                supplied
                    .canonicalize()
                    .map_err(|source| SandboxError::InvalidWriteRoot {
                        path: supplied.to_path_buf(),
                        source,
                    })?;
            let metadata =
                canonical
                    .metadata()
                    .map_err(|source| SandboxError::InvalidWriteRoot {
                        path: supplied.to_path_buf(),
                        source,
                    })?;
            roots.insert(canonical, RootKind::for_metadata(&metadata));
        }
        if roots.is_empty() {
            return Err(SandboxError::NoWriteRoots);
        }
        let (write_roots, write_root_kinds) = roots.into_iter().unzip();
        Ok(Self {
            write_roots,
            write_root_kinds,
            read_roots: None,
            read_root_kinds: None,
            network,
        })
    }

    /// Canonical roots to which writes may be made.
    #[must_use]
    pub fn write_roots(&self) -> &[PathBuf] {
        &self.write_roots
    }

    /// Adds these caller-trusted read roots to intrinsic runtime/code roots and
    /// writable roots. On macOS this retains the existing narrower-read policy;
    /// on Linux the Landlock backend also grants its reviewed system roots.
    /// Sensitive credential paths beneath the user's home remain excluded.
    ///
    /// # Errors
    ///
    /// Returns an error when a supplied root cannot be canonicalized.
    pub fn with_read_roots<I, P>(mut self, read_roots: I) -> Result<Self, SandboxError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut roots = BTreeMap::new();
        for root in read_roots {
            let supplied = root.as_ref();
            let canonical =
                supplied
                    .canonicalize()
                    .map_err(|source| SandboxError::InvalidReadRoot {
                        path: supplied.to_path_buf(),
                        source,
                    })?;
            let metadata =
                canonical
                    .metadata()
                    .map_err(|source| SandboxError::InvalidReadRoot {
                        path: supplied.to_path_buf(),
                        source,
                    })?;
            roots.insert(canonical, RootKind::for_metadata(&metadata));
        }
        let (read_roots, read_root_kinds) = roots.into_iter().unzip();
        self.read_roots = Some(read_roots);
        self.read_root_kinds = Some(read_root_kinds);
        Ok(self)
    }

    /// Exact roots visible to a read-restricted child, if narrowing was requested.
    #[must_use]
    pub fn read_roots(&self) -> Option<&[PathBuf]> {
        self.read_roots.as_deref()
    }

    /// Network authority for the child.
    #[must_use]
    pub const fn network(&self) -> &NetworkPolicy {
        &self.network
    }

    /// Returns a policy with identical roots and different network authority.
    #[must_use]
    pub fn with_network(&self, network: NetworkPolicy) -> Self {
        Self {
            write_roots: self.write_roots.clone(),
            write_root_kinds: self.write_root_kinds.clone(),
            read_roots: self.read_roots.clone(),
            read_root_kinds: self.read_root_kinds.clone(),
            network,
        }
    }

    /// Removes every filesystem write grant while preserving read and network
    /// policy for supervised background commands.
    #[must_use]
    pub fn read_only(&self) -> Self {
        Self {
            write_roots: Vec::new(),
            write_root_kinds: Vec::new(),
            read_roots: self.read_roots.clone(),
            read_root_kinds: self.read_root_kinds.clone(),
            network: self.network.clone(),
        }
    }
}

/// One executable and argument vector.  No shell interpolation is involved.
#[derive(Debug)]
pub struct LaunchPlan {
    /// Program to spawn.
    pub program: PathBuf,
    /// Exact arguments passed to the program.
    pub args: Vec<OsString>,
    /// User-visible degradation warnings.  An enforceable plan never carries a
    /// warning; unsupported configurations return an error instead.
    pub warnings: Vec<String>,
    /// Open descriptor pinning the exact already-running Linux engine inode
    /// until the namespace launcher crosses `exec(2)`.
    #[cfg(target_os = "linux")]
    helper_pin: Option<std::fs::File>,
}

impl LaunchPlan {
    /// Transfers the Linux helper inode pin to the process launcher.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn take_helper_pin(&mut self) -> Option<std::fs::File> {
        self.helper_pin.take()
    }
}

/// Strength of sandbox support on the current host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxSupport {
    /// Native enforcement is available.
    Enforced,
    /// No safe implementation is available; callers must prompt for every
    /// command or refuse execution.
    Unavailable,
}

/// Result of the local egress proxy's pre-connect policy gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EgressDecision {
    /// The proxy may connect to the already-resolved public address.
    Allowed,
    /// Destination is public but needs a user approval before the allow-list is
    /// extended.
    ApprovalRequired,
    /// A local, private, reserved, or unresolved destination is forbidden.
    HardDenied,
}

/// Address and domain policy evaluated by the local egress proxy immediately
/// before every upstream connection.
///
/// Callers must supply the complete DNS answer set and then connect to one of
/// those same pinned addresses.  A mixed public/private response is denied in
/// full, preventing DNS rebinding from bypassing the local-address rail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressPolicy {
    allowed_domains: BTreeSet<String>,
    allow_private_destinations: bool,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self::new([
            "crates.io",
            "static.crates.io",
            "index.crates.io",
            "registry.npmjs.org",
            "pypi.org",
            "files.pythonhosted.org",
            "proxy.golang.org",
            "repo.maven.apache.org",
            "rubygems.org",
        ])
    }
}

impl EgressPolicy {
    /// Creates a domain allow-list.  Invalid domains are ignored, making a bad
    /// entry fail closed rather than matching unexpectedly.
    #[must_use]
    pub fn new<I, S>(domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            allowed_domains: domains
                .into_iter()
                .filter_map(|domain| normalize_domain(domain.as_ref()))
                .collect(),
            allow_private_destinations: false,
        }
    }

    /// Adds a user-approved public domain at once/session/always scope.  Scope
    /// persistence belongs to the permission engine; the proxy gate consumes
    /// the resulting effective set through this same API.
    pub fn allow_domain(&mut self, domain: &str) -> bool {
        let Some(domain) = normalize_domain(domain) else {
            return false;
        };
        self.allowed_domains.insert(domain);
        true
    }

    /// Explicit global SSRF rail opt-out.  This is intentionally not expressible
    /// as a per-domain approval.
    #[must_use]
    pub const fn with_private_destinations(mut self, allow: bool) -> Self {
        self.allow_private_destinations = allow;
        self
    }

    /// Evaluates one post-DNS proxy connection.
    #[must_use]
    pub fn evaluate(&self, host: &str, addresses: &[IpAddr]) -> EgressDecision {
        if addresses.is_empty()
            || (!self.allow_private_destinations
                && addresses
                    .iter()
                    .copied()
                    .any(|address| !is_public_ip(address)))
        {
            return EgressDecision::HardDenied;
        }
        let Some(host) = normalize_domain(host) else {
            return EgressDecision::HardDenied;
        };
        if self.allowed_domains.iter().any(|allowed| {
            host == *allowed
                || host
                    .strip_suffix(allowed)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        }) {
            EgressDecision::Allowed
        } else {
            EgressDecision::ApprovalRequired
        }
    }
}

fn normalize_domain(domain: &str) -> Option<String> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    (!domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }))
    .then_some(domain)
}

/// Canonicalizes one requested egress domain using the proxy policy grammar.
#[must_use]
pub fn normalize_egress_domain(domain: &str) -> Option<String> {
    normalize_domain(domain)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..=19).contains(&b))
        || a >= 240)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    if segments[..6] == [0, 0, 0, 0, 0, 0] {
        return is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        return segments[2..6] == [0, 0, 0, 0]
            && is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    if segments[0] == 0x2002 {
        return is_public_v4(embedded_ipv4(segments[1], segments[2]));
    }
    if matches!(segments[4], 0 | 0x0200) && segments[5] == 0x5efe {
        return is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || (segments[0] == 0x2001 && matches!(segments[1], 0 | 0x0db8)))
}

fn embedded_ipv4(high: u16, low: u16) -> Ipv4Addr {
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

/// A deterministic capability probe suitable for `rw doctor` and policy
/// selection.  Probing never weakens a launch policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxCapability {
    /// Current enforcement status.
    pub support: SandboxSupport,
    /// Selected implementation.
    pub backend: &'static str,
    /// Loud degradation warning, if unavailable.
    pub warning: Option<String>,
}

/// Probes the current operating system for an enforceable sandbox backend.
#[must_use]
pub fn probe() -> SandboxCapability {
    #[cfg(target_os = "macos")]
    {
        if Path::new("/usr/bin/sandbox-exec").is_file() {
            return SandboxCapability {
                support: SandboxSupport::Enforced,
                backend: "seatbelt",
                warning: None,
            };
        }
        unavailable("macOS Seatbelt launcher /usr/bin/sandbox-exec is missing")
    }
    #[cfg(target_os = "linux")]
    {
        use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
        use std::process::{Command, Stdio};

        let Some(unshare) = audited_linux_tool(&["/usr/bin/unshare"]) else {
            return unavailable("trusted /usr/bin/unshare is unavailable");
        };
        let available = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V3))
            .and_then(Ruleset::create)
            .is_ok();
        let namespaces = available
            && Command::new(unshare)
                .args([
                    "--user",
                    "--map-current-user",
                    "--net",
                    "--pid",
                    "--fork",
                    "--kill-child",
                    "--",
                    "/usr/bin/true",
                ])
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
        if namespaces {
            SandboxCapability {
                support: SandboxSupport::Enforced,
                backend: "user+netns+landlock-v3+seccomp",
                warning: None,
            }
        } else {
            unavailable(
                "Landlock V3 or unprivileged user/network namespaces are unavailable; commands require prompts and sandboxed execution is refused",
            )
        }
    }
    #[cfg(target_os = "windows")]
    {
        return unavailable(
            "native Windows sandboxing is unavailable; use WSL or approve every command",
        );
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        unavailable("OS sandboxing is unavailable on this platform; approve every command")
    }
}

/// Probes the stricter per-command policy-egress transport independently from
/// the filesystem sandbox. The Linux probe executes a disposable user/network
/// namespace setup so administrators get a distinct fail-closed diagnosis when
/// user namespaces are disabled by kernel, container, or LSM policy.
#[must_use]
pub fn probe_policy_egress() -> SandboxCapability {
    #[cfg(target_os = "macos")]
    {
        probe()
    }
    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};

        let Some(unshare) = audited_linux_tool(&["/usr/bin/unshare"]) else {
            return unavailable("trusted /usr/bin/unshare is unavailable");
        };
        let Some(ip) = audited_linux_tool(&["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip"]) else {
            return unavailable("trusted iproute2 is unavailable");
        };
        let status = Command::new(unshare)
            .args([
                "--user",
                "--map-current-user",
                "--net",
                "--pid",
                "--fork",
                "--kill-child",
                "--",
            ])
            .arg(ip)
            .args(["link", "set", "dev", "lo", "up"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok_and(|status| status.success()) {
            SandboxCapability {
                support: SandboxSupport::Enforced,
                backend: "user+netns-loopback-relay",
                warning: None,
            }
        } else {
            unavailable(
                "unprivileged Linux user/network namespaces are blocked; policy egress is refused",
            )
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        unavailable("policy egress is unavailable on this platform")
    }
}

fn unavailable(message: &str) -> SandboxCapability {
    SandboxCapability {
        support: SandboxSupport::Unavailable,
        backend: "none",
        warning: Some(message.to_owned()),
    }
}

/// Builds an OS-native launch plan for a shell command.
///
/// `helper_executable` is the trusted Rottweiler executable that recognizes
/// [`HELPER_ARG`] before parsing user-facing CLI options.  It is used only on
/// Linux; macOS directly launches Seatbelt.
///
/// # Errors
///
/// Fails closed when the requested network policy cannot be enforced or the
/// platform has no sandbox implementation.
pub fn shell_launch_plan(
    policy: &SandboxPolicy,
    helper_executable: &Path,
    shell: &Path,
    shell_args: &[OsString],
) -> Result<LaunchPlan, SandboxError> {
    #[cfg(target_os = "macos")]
    {
        let _ = helper_executable;
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
        for (index, root) in sensitive_read_roots().iter().enumerate() {
            args.push(OsString::from("-D"));
            let mut definition = OsString::from(format!("RW_SECRET_{index}="));
            definition.push(root.as_os_str());
            args.push(definition);
        }
        args.push(shell.as_os_str().to_owned());
        args.extend_from_slice(shell_args);
        Ok(LaunchPlan {
            program: PathBuf::from("/usr/bin/sandbox-exec"),
            args,
            warnings: Vec::new(),
        })
    }
    #[cfg(target_os = "linux")]
    {
        if matches!(
            &policy.network,
            NetworkPolicy::PolicyProxy {
                relay_path: None,
                ..
            }
        ) {
            return Err(SandboxError::PolicyProxyUnavailable);
        }
        let (helper_executable, helper_pin) = pin_linux_helper(helper_executable)?;
        let encoded = serde_json::to_os_string(policy)?;
        let mut args = vec![OsString::from(HELPER_ARG), encoded];
        args.push(shell.as_os_str().to_owned());
        args.extend_from_slice(shell_args);
        if let NetworkPolicy::PolicyProxy {
            port,
            relay_path: Some(relay_path),
        } = &policy.network
        {
            if *port == 0 || !relay_path.is_absolute() {
                return Err(SandboxError::PolicyProxyUnavailable);
            }
            let unshare = audited_linux_tool(&["/usr/bin/unshare"])
                .ok_or(SandboxError::PolicyProxyUnavailable)?;
            let mut unshare_args = linux_namespace_args(&helper_executable);
            unshare_args.extend(args);
            return Ok(LaunchPlan {
                program: unshare,
                args: unshare_args,
                warnings: Vec::new(),
                helper_pin: Some(helper_pin),
            });
        }
        let unshare = audited_linux_tool(&["/usr/bin/unshare"]).ok_or_else(|| {
            SandboxError::Unavailable("trusted /usr/bin/unshare is unavailable".to_owned())
        })?;
        let mut unshare_args = linux_namespace_args(&helper_executable);
        unshare_args.extend(args);
        Ok(LaunchPlan {
            program: unshare,
            args: unshare_args,
            warnings: Vec::new(),
            helper_pin: Some(helper_pin),
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (policy, helper_executable, shell, shell_args);
        Err(SandboxError::Unavailable(
            probe()
                .warning
                .unwrap_or_else(|| "OS sandbox is unavailable".to_owned()),
        ))
    }
}

#[cfg(target_os = "linux")]
fn linux_namespace_args(helper: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--user"),
        OsString::from("--map-current-user"),
        OsString::from("--net"),
        OsString::from("--pid"),
        OsString::from("--fork"),
        OsString::from("--kill-child"),
        OsString::from("--"),
        helper.as_os_str().to_owned(),
    ]
}

#[cfg(target_os = "linux")]
fn pin_linux_helper(helper: &Path) -> Result<(PathBuf, std::fs::File), SandboxError> {
    use std::fs::File;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::MetadataExt as _;

    let canonical = helper
        .canonicalize()
        .map_err(|_| SandboxError::UntrustedHelper)?;
    if canonical != helper {
        return Err(SandboxError::UntrustedHelper);
    }
    let before = canonical
        .metadata()
        .map_err(|_| SandboxError::UntrustedHelper)?;
    if !before.is_file() || before.mode() & 0o111 == 0 {
        return Err(SandboxError::UntrustedHelper);
    }
    let file = File::open(&canonical).map_err(|_| SandboxError::UntrustedHelper)?;
    let pinned = file.metadata().map_err(|_| SandboxError::UntrustedHelper)?;
    let running = Path::new("/proc/self/exe")
        .metadata()
        .map_err(|_| SandboxError::UntrustedHelper)?;
    if (before.dev(), before.ino()) != (pinned.dev(), pinned.ino())
        || (pinned.dev(), pinned.ino()) != (running.dev(), running.ino())
    {
        return Err(SandboxError::UntrustedHelper);
    }
    let descriptor = file.as_raw_fd();
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())
        .map_err(|_| SandboxError::UntrustedHelper)?;
    Ok((PathBuf::from(format!("/proc/self/fd/{descriptor}")), file))
}

#[cfg(target_os = "linux")]
fn audited_linux_tool(candidates: &[&str]) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;

    candidates.iter().find_map(|candidate| {
        let expected = Path::new(candidate);
        let canonical = expected.canonicalize().ok()?;
        if canonical != expected {
            return None;
        }
        let metadata = canonical.metadata().ok()?;
        (metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0)
            .then_some(canonical)
    })
}

#[cfg(target_os = "macos")]
fn seatbelt_profile(policy: &SandboxPolicy) -> String {
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
            "(deny network-outbound (require-not (remote ip \"localhost:{port}\"))) (deny network-bind) (deny network-inbound)"
        ),
    };
    let read_rule = readable.map_or_else(String::new, |readable| {
        format!(
            "(deny file-read* (require-not (require-any (literal \"/\") (literal \"/dev/null\") {writable} {readable})))"
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
        "(version 1) (allow default) {read_rule} {secret_rule} (deny file-write* (require-not (require-any (literal \"/dev/null\") {writable}))) {network}"
    )
}

#[cfg(target_os = "macos")]
fn sensitive_read_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    else {
        return Vec::new();
    };
    SENSITIVE_HOME_SUFFIXES
        .iter()
        .map(|suffix| home.join(suffix))
        .collect()
}

/// Handles a Linux sandbox-helper invocation and replaces the current process
/// with the requested command after Landlock and seccomp are active.
///
/// Returns `Ok(false)` for normal public CLI invocations.  On a helper
/// invocation this function either returns an error before executing anything
/// or calls `exec(2)` and never returns.
///
/// # Errors
///
/// Fails closed on malformed internal arguments, unsupported Landlock, filter
/// installation failure, or `exec(2)` failure.
pub fn maybe_run_helper<I, S>(args: I) -> Result<bool, SandboxError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    if args.get(1).and_then(|value| value.to_str()) != Some(HELPER_ARG) {
        return Ok(false);
    }
    #[cfg(target_os = "linux")]
    {
        linux::run_helper(&args).map(|never| match never {})
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        Err(SandboxError::Unavailable(
            "the internal sandbox helper is Linux-only".to_owned(),
        ))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{BTreeMap, BTreeSet};
    use std::convert::TryInto as _;
    use std::io;
    use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
    use std::os::fd::AsFd as _;
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
        ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus,
    };
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };

    use super::{
        NetworkPolicy, OsString, RootKind, SENSITIVE_HOME_SUFFIXES, SandboxError, SandboxPolicy,
        audited_linux_tool, serde_json,
    };

    /// Linux's default compatibility roots. These are deliberately explicit:
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
        if let Some(helper_pin) = helper_pin {
            let helper_pin: i32 = helper_pin
                .try_into()
                .map_err(|_| SandboxError::MalformedHelper)?;
            // The descriptor was validated from /proc/self/fd and remains
            // open in this process. CLOEXEC closes it atomically when the
            // target replaces the helper, without shell-parsing an arbitrary
            // multi-digit descriptor number.
            nix::fcntl::fcntl(
                helper_pin,
                nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
            )
            .map_err(sandbox_backend)?;
        }
        let mut command = Command::new(program);
        command.args(args);
        Ok(command)
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
        raise_loopback()?;
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(SandboxError::Proxy)?;
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

        install_landlock(policy, program)?;
        install_network_floor(true)?;
        let status = command_without_helper_pin(program, args, helper_pin)?
            .status()
            .map_err(SandboxError::Exec)?;
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

    fn raise_loopback() -> Result<(), SandboxError> {
        let ip =
            audited_linux_tool(&["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip"]).ok_or_else(|| {
                SandboxError::Unavailable(
                    "Linux policy egress requires a trusted iproute2 executable".to_owned(),
                )
            })?;
        let status = Command::new(ip)
            .args(["link", "set", "dev", "lo", "up"])
            .env_clear()
            .status()
            .map_err(SandboxError::Exec)?;
        if !status.success() {
            return Err(SandboxError::Unavailable(
                "Linux network namespace loopback setup failed".to_owned(),
            ));
        }
        Ok(())
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
        for root in SYSTEM_READ_ROOTS {
            collect_system_read_root(Path::new(root), &homes, &mut read_grants)?;
        }
        match (&policy.read_roots, &policy.read_root_kinds) {
            (Some(read_roots), Some(read_root_kinds))
                if read_roots.len() == read_root_kinds.len() =>
            {
                for (root, kind) in read_roots.iter().zip(read_root_kinds) {
                    collect_authorized_read_root(root, *kind, &sensitive, &mut read_grants)?;
                }
            }
            (None, None) => {
                if let Some(program) = absolute_existing_root(Path::new(program))? {
                    collect_authorized_read_root(
                        &program.0,
                        program.1,
                        &sensitive,
                        &mut read_grants,
                    )?;
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

    fn linux_homes() -> Vec<PathBuf> {
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

    fn sensitive_linux_roots(home: &Path) -> Vec<PathBuf> {
        let mut roots = BTreeSet::new();
        for suffix in SENSITIVE_HOME_SUFFIXES {
            let lexical = home.join(suffix);
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

    fn open_landlock_root(root: &Path, expected: RootKind) -> Result<PathFd, SandboxError> {
        // Open first and classify the pinned descriptor.  The same descriptor
        // becomes the Landlock rule parent, so a concurrent path swap cannot
        // turn file-only authority into directory-wide authority.
        let root = PathFd::new(root).map_err(sandbox_backend)?;
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
        denied.insert(libc::SYS_socketpair, local_stream_pair_rules()?);
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

    fn local_stream_pair_rules() -> Result<Vec<SeccompRule>, SandboxError> {
        // Local full-duplex stream pairs are process-local IPC used by Tokio
        // and MCP transports; they cannot cross the empty network namespace.
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
        let mut invalid_unix_stream_type = vec![
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
        ] {
            invalid_unix_stream_type.push(
                SeccompCondition::new(
                    1,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    u64::try_from(allowed_type).map_err(sandbox_backend)?,
                )
                .map_err(sandbox_backend)?,
            );
        }
        let invalid_unix_stream_type =
            SeccompRule::new(invalid_unix_stream_type).map_err(sandbox_backend)?;
        let nonzero_pair_protocol = SeccompRule::new(vec![
            SeccompCondition::new(2, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, 0)
                .map_err(sandbox_backend)?,
        ])
        .map_err(sandbox_backend)?;
        Ok(vec![
            non_unix_pair,
            invalid_unix_stream_type,
            nonzero_pair_protocol,
        ])
    }

    fn sandbox_backend(error: impl std::fmt::Display) -> SandboxError {
        SandboxError::Backend(error.to_string())
    }
}

/// Sandbox policy or backend failure.  Messages never include command text or
/// environment contents.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// An explicit corporate proxy URL failed structural validation.
    #[error("configured corporate proxy URL is invalid")]
    InvalidProxy,
    /// A caller-supplied target DNS pin was structurally invalid.
    #[error("validated egress DNS pin is invalid")]
    InvalidEgressPin,
    /// A writable root could not be canonicalized.
    #[error("sandbox write root is invalid: {path}")]
    InvalidWriteRoot {
        /// Supplied path.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A readable root could not be canonicalized.
    #[error("sandbox read root is invalid: {path}")]
    InvalidReadRoot {
        /// Supplied path.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// At least one writable root is required.
    #[error("sandbox requires at least one writable root")]
    NoWriteRoots,
    /// A root changed between policy approval and descriptor pinning.
    #[error("sandbox filesystem root changed type before enforcement")]
    RootTypeChanged,
    /// Platform support is absent or too weak.
    #[error("{0}")]
    Unavailable(String),
    /// The requested proxy-only route could not be constructed safely.
    #[error("sandbox policy-proxy networking is unavailable; refusing ambient network access")]
    PolicyProxyUnavailable,
    /// Local egress proxy lifecycle failure.
    #[error("local egress proxy failed: {0}")]
    Proxy(std::io::Error),
    /// Internal helper arguments were malformed.
    #[error("malformed internal sandbox-helper invocation")]
    MalformedHelper,
    /// The Linux sandbox helper could not be pinned to a trusted executable
    /// inode.
    #[error("sandbox helper executable is not trusted")]
    UntrustedHelper,
    /// JSON profile encoding failed.
    #[error("sandbox profile encoding failed")]
    ProfileEncoding(#[from] ::serde_json::Error),
    /// Native backend setup failed.
    #[error("sandbox backend failed: {0}")]
    Backend(String),
    /// Final process replacement failed.
    #[error("sandboxed command could not start: {0}")]
    Exec(std::io::Error),
}

#[cfg(target_os = "linux")]
mod serde_json {
    use std::ffi::{OsStr, OsString};

    use serde::{Serialize, de::DeserializeOwned};

    pub(super) fn to_os_string<T: Serialize>(value: &T) -> Result<OsString, ::serde_json::Error> {
        ::serde_json::to_string(value).map(OsString::from)
    }

    pub(super) fn from_os_str<T: DeserializeOwned>(
        value: &OsStr,
    ) -> Result<T, ::serde_json::Error> {
        let text = value.to_str().ok_or_else(|| {
            <::serde_json::Error as serde::de::Error>::custom("profile is not UTF-8")
        })?;
        ::serde_json::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    #[cfg(target_os = "macos")]
    use std::fs;
    #[cfg(target_os = "macos")]
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn policy_canonicalizes_deduplicates_and_requires_roots() {
        let directory = tempdir().expect("temporary directory");
        let policy = SandboxPolicy::new([directory.path(), directory.path()], NetworkPolicy::Deny)
            .expect("policy");
        assert_eq!(policy.write_roots().len(), 1);
        assert!(SandboxPolicy::new(Vec::<PathBuf>::new(), NetworkPolicy::Deny).is_err());
        assert!(
            SandboxPolicy::new([directory.path().join("missing")], NetworkPolicy::Deny).is_err()
        );
    }

    #[test]
    fn helper_marker_is_unambiguous_and_normal_invocations_are_ignored() {
        assert!(!maybe_run_helper(["rw", "serve"]).expect("normal invocation"));
        #[cfg(not(target_os = "linux"))]
        assert!(maybe_run_helper(["rw", HELPER_ARG]).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn running_helper_is_pinned_even_inside_write_root_and_injected_copy_is_refused() {
        use std::fs;

        let executable = std::env::current_exe().expect("current executable");
        let writable_target = executable.parent().expect("target directory");
        let policy = SandboxPolicy::new([writable_target], NetworkPolicy::Deny).expect("policy");
        let mut plan = shell_launch_plan(&policy, &executable, Path::new("/usr/bin/true"), &[])
            .expect("self-hosted launch plan");
        assert!(plan.take_helper_pin().is_some());
        assert!(plan.args.iter().any(|argument| {
            argument
                .to_str()
                .is_some_and(|argument| argument.starts_with("/proc/self/fd/"))
        }));
        for required in ["--net", "--pid", "--fork", "--kill-child"] {
            assert!(plan.args.iter().any(|argument| argument == required));
        }

        let replacement_dir = tempdir().expect("replacement directory");
        let injected = replacement_dir.path().join("injected-helper");
        fs::copy(&executable, &injected).expect("injected helper copy");
        assert!(matches!(
            shell_launch_plan(&policy, &injected, Path::new("/usr/bin/true"), &[]),
            Err(SandboxError::UntrustedHelper)
        ));
    }

    #[test]
    fn network_grant_fails_closed_until_the_policy_proxy_is_present() {
        let directory = tempdir().expect("temporary directory");
        let policy = SandboxPolicy::new(
            [directory.path()],
            NetworkPolicy::PolicyProxy {
                port: 3128,
                relay_path: None,
            },
        )
        .expect("policy");
        #[cfg(target_os = "macos")]
        {
            assert!(matches!(
                shell_launch_plan(&policy, Path::new("rw"), Path::new("/bin/sh"), &[]),
                Err(SandboxError::PolicyProxyUnavailable)
            ));
            let proxy = SupervisedEgressProxy::start(EgressPolicy::default()).expect("proxy");
            let owned_policy = SandboxPolicy::new(
                [directory.path()],
                NetworkPolicy::PolicyProxy {
                    port: proxy.address().port(),
                    relay_path: None,
                },
            )
            .expect("owned policy");
            assert!(
                shell_launch_plan(&owned_policy, Path::new("rw"), Path::new("/bin/sh"), &[])
                    .is_ok()
            );
            drop(proxy);
            assert!(matches!(
                shell_launch_plan(&owned_policy, Path::new("rw"), Path::new("/bin/sh"), &[]),
                Err(SandboxError::PolicyProxyUnavailable)
            ));
        }
        #[cfg(not(target_os = "macos"))]
        assert!(matches!(
            shell_launch_plan(&policy, Path::new("rw"), Path::new("/bin/sh"), &[]),
            Err(SandboxError::PolicyProxyUnavailable)
        ));
    }

    #[test]
    fn egress_gate_requires_domain_approval_and_hard_denies_ssrf_answers() {
        let public: IpAddr = "1.1.1.1".parse().expect("public IP");
        let private: IpAddr = "169.254.169.254".parse().expect("metadata IP");
        let mut policy = EgressPolicy::default();
        assert_eq!(
            policy.evaluate("registry.npmjs.org", &[public]),
            EgressDecision::Allowed
        );
        assert_eq!(
            policy.evaluate("evilregistry-npmjs.org", &[public]),
            EgressDecision::ApprovalRequired
        );
        assert_eq!(
            policy.evaluate("example.com", &[public]),
            EgressDecision::ApprovalRequired
        );
        assert!(policy.allow_domain("example.com"));
        assert!(policy.allow_domain("example.com"));
        assert!(policy.allow_domain("registry.npmjs.org"));
        assert!(!policy.allow_domain("https://example.com/path"));
        assert_eq!(
            policy.evaluate("cdn.example.com", &[public]),
            EgressDecision::Allowed
        );
        for addresses in [&[][..], &[private][..], &[public, private][..]] {
            assert_eq!(
                policy.evaluate("example.com", addresses),
                EgressDecision::HardDenied
            );
        }
        assert_eq!(
            policy
                .clone()
                .with_private_destinations(true)
                .evaluate("example.com", &[private]),
            EgressDecision::Allowed
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_broad_read_mode_explicitly_denies_user_credential_roots() {
        let directory = tempdir().expect("temporary directory");
        let policy = SandboxPolicy::new([directory.path()], NetworkPolicy::Deny).expect("policy");
        let plan = shell_launch_plan(&policy, Path::new("rw"), Path::new("/bin/true"), &[])
            .expect("launch plan");
        let profile = plan
            .args
            .get(1)
            .and_then(|value| value.to_str())
            .expect("profile");
        let sensitive = sensitive_read_roots();
        assert!(!sensitive.is_empty());
        for (index, root) in sensitive.iter().enumerate() {
            assert!(profile.contains(&format!("(subpath (param \"RW_SECRET_{index}\"))")));
            let expected = {
                let mut value = OsString::from(format!("RW_SECRET_{index}="));
                value.push(root);
                value
            };
            assert!(plan.args.contains(&expected));
        }
        assert!(profile.contains("(deny file-read* (require-any"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_enforces_write_roots_and_network_denial_at_the_syscall() {
        let directory = tempdir().expect("temporary directory");
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let outside = directory.path().join("outside");
        let policy = SandboxPolicy::new([&workspace], NetworkPolicy::Deny).expect("policy");
        let script = r#"
printf inside > "$1/inside"
if printf outside > "$2" 2>/dev/null; then exit 91; fi
python3 -c 'import errno,socket,sys; s=socket.socket();
try: s.connect(("127.0.0.1",9))
except OSError as e: sys.exit(0 if e.errno in (errno.EPERM,errno.EACCES) else 93)
sys.exit(92)'
"#;
        let args = [
            OsString::from("-c"),
            OsString::from(script),
            OsString::from("sandbox-test"),
            workspace.as_os_str().to_owned(),
            outside.as_os_str().to_owned(),
        ];
        let plan = shell_launch_plan(&policy, Path::new("rw"), Path::new("/bin/sh"), &args)
            .expect("launch plan");
        let status = Command::new(&plan.program)
            .args(&plan.args)
            .status()
            .expect("sandbox-exec status");
        assert!(status.success(), "sandbox probe exited {status}");
        assert_eq!(
            fs::read_to_string(workspace.join("inside")).expect("inside"),
            "inside"
        );
        assert!(!outside.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn second_root_is_writable_while_its_parent_remains_blocked() {
        let directory = tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let parent = directory.path().join("adjacent");
        let second = parent.join("second");
        fs::create_dir(&first).expect("first");
        fs::create_dir_all(&second).expect("second");
        let blocked = parent.join("blocked");
        let policy = SandboxPolicy::new([&first, &second], NetworkPolicy::Deny).expect("policy");
        let args = [
            OsString::from("-c"),
            OsString::from(
                "printf one > \"$1/a\"; printf two > \"$2/b\"; ! printf bad > \"$3\" 2>/dev/null",
            ),
            OsString::from("multi-root-test"),
            first.as_os_str().to_owned(),
            second.as_os_str().to_owned(),
            blocked.as_os_str().to_owned(),
        ];
        let plan = shell_launch_plan(&policy, Path::new("rw"), Path::new("/bin/sh"), &args)
            .expect("launch plan");
        assert!(
            Command::new(plan.program)
                .args(plan.args)
                .status()
                .expect("status")
                .success()
        );
        assert!(first.join("a").is_file());
        assert!(second.join("b").is_file());
        assert!(!blocked.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_proxy_route_is_exact_and_direct_socket_bypasses_get_eperm() {
        use std::net::{Ipv4Addr, TcpListener};

        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("upstream");
        upstream
            .set_nonblocking(true)
            .expect("nonblocking upstream");
        let upstream_address = upstream.local_addr().expect("upstream address");
        let proxy = SupervisedEgressProxy::start(
            EgressPolicy::new(["localhost"]).with_private_destinations(true),
        )
        .expect("proxy");
        let directory = tempdir().expect("temporary directory");
        let policy = SandboxPolicy::new(
            [directory.path()],
            NetworkPolicy::PolicyProxy {
                port: proxy.address().port(),
                relay_path: None,
            },
        )
        .expect("policy");
        let script = r"import errno, socket, sys
proxy_port = PROXY_PORT; upstream_port = UPSTREAM_PORT
s = socket.create_connection(('127.0.0.1', proxy_port), timeout=2)
s.sendall(('CONNECT localhost:%d HTTP/1.1\r\nHost: localhost:%d\r\n\r\n' % (upstream_port, upstream_port)).encode())
if b'200 Connection Established' not in s.recv(1024): sys.exit(90)
s.close()
for target in [('127.0.0.1', upstream_port), ('127.0.0.1', proxy_port + 1), ('1.1.1.1', proxy_port)]:
    candidate = socket.socket(); candidate.settimeout(1)
    try: candidate.connect(target)
    except OSError as error:
        if error.errno not in (errno.EPERM, errno.EACCES): sys.exit(91)
    else: sys.exit(92)
"
        .replace("PROXY_PORT", &proxy.address().port().to_string())
        .replace("UPSTREAM_PORT", &upstream_address.port().to_string());
        let args = [OsString::from("-c"), OsString::from(script)];
        let plan = shell_launch_plan(&policy, Path::new("rw"), Path::new("python3"), &args)
            .expect("proxy-only launch plan");
        let status = Command::new(plan.program)
            .args(plan.args)
            .status()
            .expect("sandbox proxy probe");
        assert!(status.success(), "proxy-route probe exited {status}");
        drop(proxy);
        assert!(matches!(
            upstream.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }
}
