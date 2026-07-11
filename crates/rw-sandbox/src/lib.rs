//! OS-native sandbox policy, capability probing, and launch-plan construction.
//!
//! The crate deliberately does not execute commands.  It turns a reviewed
//! policy into an argv-only launch plan consumed by `rw-tools`, and exposes the
//! Linux helper entry point used immediately before `exec(2)`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod proxy;
pub use proxy::SupervisedEgressProxy;

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "sandbox";

/// The internal argv marker handled before the public CLI parser starts.
pub const HELPER_ARG: &str = "__rottweiler-sandbox-helper";

/// Network authority granted to a sandboxed process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// All socket connection and bind attempts are denied.
    #[default]
    Deny,
    /// Egress is possible only through a separately supervised policy proxy.
    ///
    /// Transparent proxy plumbing is not yet implemented.  Launch planning
    /// therefore fails closed for this value instead of silently granting
    /// ambient network access.
    PolicyProxy {
        /// Loopback port of the supervised host-side proxy.
        port: u16,
    },
}

/// Per-invocation filesystem and network authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    write_roots: Vec<PathBuf>,
    network: NetworkPolicy,
}

impl SandboxPolicy {
    /// Creates a policy after resolving every writable root to an existing,
    /// absolute filesystem object.
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
        let mut roots = BTreeSet::new();
        for root in write_roots {
            let supplied = root.as_ref();
            let canonical =
                supplied
                    .canonicalize()
                    .map_err(|source| SandboxError::InvalidWriteRoot {
                        path: supplied.to_path_buf(),
                        source,
                    })?;
            roots.insert(canonical);
        }
        if roots.is_empty() {
            return Err(SandboxError::NoWriteRoots);
        }
        Ok(Self {
            write_roots: roots.into_iter().collect(),
            network,
        })
    }

    /// Canonical roots to which writes may be made.
    #[must_use]
    pub fn write_roots(&self) -> &[PathBuf] {
        &self.write_roots
    }

    /// Network authority for the child.
    #[must_use]
    pub const fn network(&self) -> NetworkPolicy {
        self.network
    }

    /// Returns a policy with identical roots and different network authority.
    #[must_use]
    pub fn with_network(&self, network: NetworkPolicy) -> Self {
        Self {
            write_roots: self.write_roots.clone(),
            network,
        }
    }
}

/// One executable and argument vector.  No shell interpolation is involved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPlan {
    /// Program to spawn.
    pub program: PathBuf,
    /// Exact arguments passed to the program.
    pub args: Vec<OsString>,
    /// User-visible degradation warnings.  An enforceable plan never carries a
    /// warning; unsupported configurations return an error instead.
    pub warnings: Vec<String>,
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

        let available = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V3))
            .and_then(Ruleset::create)
            .is_ok();
        if available {
            SandboxCapability {
                support: SandboxSupport::Enforced,
                backend: "landlock-v3+seccomp",
                warning: None,
            }
        } else {
            unavailable(
                "Landlock V3 is not fully available; commands require prompts and sandboxed execution is refused",
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
        if matches!(policy.network, NetworkPolicy::PolicyProxy { .. }) {
            return Err(SandboxError::PolicyProxyUnavailable);
        }
        let encoded = serde_json::to_os_string(policy)?;
        let mut args = vec![OsString::from(HELPER_ARG), encoded];
        args.push(shell.as_os_str().to_owned());
        args.extend_from_slice(shell_args);
        Ok(LaunchPlan {
            program: helper_executable.to_path_buf(),
            args,
            warnings: Vec::new(),
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

#[cfg(target_os = "macos")]
fn seatbelt_profile(policy: &SandboxPolicy) -> String {
    let writable = (0..policy.write_roots.len())
        .map(|index| format!("(subpath (param \"RW_WRITE_{index}\"))"))
        .collect::<Vec<_>>()
        .join(" ");
    // `allow default` preserves host toolchain compatibility.  The two deny
    // rules are the security boundary: write operations outside the canonical
    // roots and all network operations are rejected by Seatbelt.
    let network = match policy.network {
        NetworkPolicy::Deny => "(deny network*)".to_owned(),
        NetworkPolicy::PolicyProxy { port } => format!(
            "(deny network-outbound (require-not (remote ip \"localhost:{port}\"))) (deny network-bind) (deny network-inbound)"
        ),
    };
    format!(
        "(version 1) (allow default) (deny file-write* (require-not (require-any (literal \"/dev/null\") {writable}))) {network}"
    )
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
    use std::collections::BTreeMap;
    use std::convert::TryInto as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;

    use landlock::{
        ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus,
    };
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};

    use super::{NetworkPolicy, OsString, SandboxError, SandboxPolicy, serde_json};

    pub(super) fn run_helper(args: &[OsString]) -> Result<std::convert::Infallible, SandboxError> {
        if args.len() < 4 {
            return Err(SandboxError::MalformedHelper);
        }
        let policy: SandboxPolicy = serde_json::from_os_str(&args[2])?;
        if policy.write_roots.is_empty() || policy.network != NetworkPolicy::Deny {
            return Err(SandboxError::MalformedHelper);
        }
        install_landlock(&policy)?;
        install_network_floor()?;
        let error = Command::new(&args[3]).args(&args[4..]).exec();
        Err(SandboxError::Exec(error))
    }

    fn install_landlock(policy: &SandboxPolicy) -> Result<(), SandboxError> {
        // V3 includes REFER and TRUNCATE.  Requiring full enforcement prevents
        // older kernels from silently leaving path-based truncate unrestricted.
        let abi = ABI::V3;
        let all = AccessFs::from_all(abi);
        let read = AccessFs::from_read(abi);
        let mut ruleset = Ruleset::default()
            .handle_access(all)
            .map_err(sandbox_backend)?
            .create()
            .map_err(sandbox_backend)?
            .add_rule(PathBeneath::new(
                PathFd::new("/").map_err(sandbox_backend)?,
                read,
            ))
            .map_err(sandbox_backend)?;
        for root in &policy.write_roots {
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    PathFd::new(root).map_err(sandbox_backend)?,
                    all,
                ))
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

    fn install_network_floor() -> Result<(), SandboxError> {
        let denied = [
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_accept,
            libc::SYS_accept4,
        ]
        .into_iter()
        .map(|syscall| (syscall, Vec::new()))
        .collect::<BTreeMap<_, _>>();
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

    fn sandbox_backend(error: impl std::fmt::Display) -> SandboxError {
        SandboxError::Backend(error.to_string())
    }
}

/// Sandbox policy or backend failure.  Messages never include command text or
/// environment contents.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// A writable root could not be canonicalized.
    #[error("sandbox write root is invalid: {path}")]
    InvalidWriteRoot {
        /// Supplied path.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// At least one writable root is required.
    #[error("sandbox requires at least one writable root")]
    NoWriteRoots,
    /// Platform support is absent or too weak.
    #[error("{0}")]
    Unavailable(String),
    /// Proxy-only networking cannot yet be constructed safely.
    #[error("sandbox policy-proxy networking is unavailable; refusing ambient network access")]
    PolicyProxyUnavailable,
    /// Local egress proxy lifecycle failure.
    #[error("local egress proxy failed: {0}")]
    Proxy(std::io::Error),
    /// Internal helper arguments were malformed.
    #[error("malformed internal sandbox-helper invocation")]
    MalformedHelper,
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

    #[test]
    fn network_grant_fails_closed_until_the_policy_proxy_is_present() {
        let directory = tempdir().expect("temporary directory");
        let policy = SandboxPolicy::new(
            [directory.path()],
            NetworkPolicy::PolicyProxy { port: 3128 },
        )
        .expect("policy");
        #[cfg(target_os = "macos")]
        assert!(shell_launch_plan(&policy, Path::new("rw"), Path::new("/bin/sh"), &[]).is_ok());
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
