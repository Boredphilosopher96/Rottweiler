use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use rw_tools::{
    BashSandboxMode, CommandSafety, MutationScope, ToolBehavior, ToolInvocationSemantics,
    classify_safe_command,
};
use rw_types::ToolCapability;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{PermissionApprovalSummary, PermissionRequest, canonical_json, hex, is_assignment};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct PermissionKey {
    pub(super) tool_name: String,
    pub(super) arguments_fingerprint: String,
    pub(super) capabilities: Vec<String>,
    pub(super) approval_fingerprint: Option<String>,
    pub(super) workspace_namespace: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct RememberedApproval {
    pub(super) id: String,
    pub(super) key: PermissionKey,
}

impl RememberedApproval {
    pub(super) fn new(scope: &str, key: PermissionKey) -> Option<Self> {
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).ok()?;
        let opaque = hex(&random);
        Some(Self {
            id: format!("{scope}:{opaque}"),
            key,
        })
    }

    pub(super) fn summary(&self) -> PermissionApprovalSummary {
        let capabilities = if self.key.capabilities.is_empty() {
            "none".to_owned()
        } else {
            self.key.capabilities.join(",")
        };
        let approval = self
            .key
            .approval_fingerprint
            .as_ref()
            .map_or("none", |_| "diff-bound");
        PermissionApprovalSummary {
            id: self.id.clone(),
            tool_name: self.key.tool_name.clone(),
            canonical_summary: format!(
                "exact-invocation=hidden capabilities={capabilities} approval={approval}"
            ),
        }
    }
}

impl PermissionKey {
    #[cfg(test)]
    pub(super) fn from_request(
        request: &PermissionRequest,
        workspace_namespace: &[String],
    ) -> Self {
        Self::from_request_with_behavior(request, workspace_namespace, ToolBehavior::Standard)
    }

    pub(super) fn from_request_with_behavior(
        request: &PermissionRequest,
        workspace_namespace: &[String],
        behavior: ToolBehavior,
    ) -> Self {
        let mut capabilities = request
            .capabilities
            .iter()
            .map(|capability| format!("{capability:?}"))
            .collect::<Vec<_>>();
        capabilities.sort();
        capabilities.dedup();
        Self {
            tool_name: request.tool_name.clone(),
            arguments_fingerprint: fingerprint(
                b"rottweiler-permission-arguments-v1\0",
                canonical_key_arguments_for(request, behavior).as_bytes(),
            ),
            capabilities,
            approval_fingerprint: request.approval_diff.as_ref().map(|diff| {
                format!(
                    "{}:{}:{}:{}",
                    diff.arguments_hash, diff.base_hash, diff.diff_hash, diff.truncated
                )
            }),
            workspace_namespace: workspace_namespace.to_vec(),
        }
    }
}

pub(super) fn fingerprint(domain: &[u8], value: &[u8]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(domain);
    hash.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hash.update(value);
    hash.finalize().to_hex().to_string()
}

pub(super) fn revoke_approvals(
    approvals: &mut BTreeSet<RememberedApproval>,
    id: Option<&str>,
) -> usize {
    let before = approvals.len();
    if let Some(id) = id {
        approvals.retain(|approval| approval.id != id);
    } else {
        approvals.clear();
    }
    before.saturating_sub(approvals.len())
}

pub(super) fn contains_approval(
    approvals: &BTreeSet<RememberedApproval>,
    key: &PermissionKey,
) -> bool {
    approvals.iter().any(|approval| &approval.key == key)
}

pub(super) fn replace_approval(
    approvals: &mut BTreeSet<RememberedApproval>,
    approval: RememberedApproval,
) {
    approvals.retain(|existing| existing.key != approval.key);
    approvals.insert(approval);
}

pub(super) fn workspace_namespace(
    roots: impl IntoIterator<Item = impl AsRef<Path>>,
) -> Vec<String> {
    let mut namespace = blake3::Hasher::new();
    namespace.update(b"rottweiler-permission-workspace-roots-v1\0");
    let mut count = 0_u64;
    for root in roots {
        let canonical =
            fs::canonicalize(root.as_ref()).unwrap_or_else(|_| root.as_ref().to_path_buf());
        let bytes = canonical.as_os_str().as_encoded_bytes();
        namespace.update(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
        namespace.update(bytes);
        count = count.saturating_add(1);
    }
    namespace.update(&count.to_le_bytes());
    vec![namespace.finalize().to_hex().to_string()]
}

pub(super) fn canonical_workspace_roots(
    roots: impl IntoIterator<Item = impl AsRef<Path>>,
) -> Vec<PathBuf> {
    roots
        .into_iter()
        .map(|root| fs::canonicalize(root.as_ref()).unwrap_or_else(|_| root.as_ref().to_path_buf()))
        .collect()
}

pub(super) fn is_auto_safe_workspace_write(
    request: &PermissionRequest,
    semantics: Option<&ToolInvocationSemantics>,
    roots: &[PathBuf],
) -> bool {
    let Some(semantics) = semantics else {
        return false;
    };
    let MutationScope::Paths(paths) = &semantics.mutation_scope else {
        return false;
    };
    if semantics.behavior != ToolBehavior::FileMutation
        || paths.is_empty()
        || roots.is_empty()
        || !request
            .capabilities
            .contains(&ToolCapability::WriteFilesystem)
        || request.capabilities.iter().any(|capability| {
            matches!(
                capability,
                ToolCapability::Execute | ToolCapability::Network
            )
        })
    {
        return false;
    }
    paths
        .iter()
        .all(|path| resolve_workspace_write_path(roots, path).is_some())
}

pub(super) fn resolve_workspace_write_path(roots: &[PathBuf], supplied: &Path) -> Option<PathBuf> {
    let candidate = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        let mut components = supplied.components();
        if components.next().is_some_and(
            |component| matches!(component, std::path::Component::Normal(name) if name == "@root"),
        ) {
            let std::path::Component::Normal(index) = components.next()? else {
                return None;
            };
            let index = index.to_str()?.parse::<usize>().ok()?;
            roots.get(index)?.join(components.collect::<PathBuf>())
        } else {
            roots.first()?.join(supplied)
        }
    };
    let canonical = canonicalize_with_missing_tail(&candidate)?;
    roots
        .iter()
        .any(|root| canonical.starts_with(root))
        .then_some(canonical)
}

pub(super) fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut tail = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in tail.iter().rev() {
                    canonical.push(component);
                }
                return Some(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tail.push(ancestor.file_name()?.to_owned());
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
}

pub(super) fn canonical_key_arguments_for(
    request: &PermissionRequest,
    behavior: ToolBehavior,
) -> String {
    let mut arguments = request.arguments.clone();
    if behavior == ToolBehavior::WebFetch
        && let Some(url) = arguments.get("url").and_then(Value::as_str)
        && let Some(origin) = canonical_webfetch_origin(url)
        && let Some(object) = arguments.as_object_mut()
    {
        object.insert("url".to_owned(), Value::String(origin));
    }
    if behavior == ToolBehavior::Shell
        && let Some(command) = arguments.get("command").and_then(Value::as_str)
        && let Some(commands) = exact_shell_identity(command, &arguments)
        && let Some(object) = arguments.as_object_mut()
    {
        object.insert(
            "command".to_owned(),
            serde_json::to_value(commands).unwrap_or(Value::Null),
        );
    }
    if let Some(object) = arguments.as_object_mut()
        && let Some(domains) = object.get("network_domains")
        && let Some(domains) = normalize_network_domains(domains)
    {
        object.insert(
            "network_domains".to_owned(),
            Value::Array(domains.into_iter().map(Value::String).collect()),
        );
    }
    canonical_json(&arguments)
}

#[derive(Serialize)]
pub(super) struct ExactShellCommand {
    operator_after: Option<String>,
    assignments: Vec<(String, String)>,
    argv: Vec<String>,
    executable: ExactExecutableIdentity,
}

#[derive(Serialize)]
pub(super) struct ExactExecutableIdentity {
    requested: String,
    resolved: Option<ResolvedExecutableIdentity>,
}

#[derive(Serialize)]
pub(super) struct ResolvedExecutableIdentity {
    canonical_path: Vec<u8>,
    content_hash: String,
    trusted_immutable: bool,
}

pub(super) fn exact_shell_identity(
    command: &str,
    arguments: &Value,
) -> Option<Vec<ExactShellCommand>> {
    let cwd = arguments
        .get("cwd")
        .and_then(Value::as_str)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let request_env = arguments.get("env").and_then(Value::as_object);
    split_compound_with_operators(command)?
        .into_iter()
        .map(|(segment, operator_after)| {
            let words = shell_words::split(&segment).ok()?;
            let executable_index = words.iter().position(|word| !is_assignment(word))?;
            let assignments = words[..executable_index]
                .iter()
                .map(|assignment| assignment.split_once('='))
                .map(|assignment| {
                    assignment.map(|(name, value)| (name.to_owned(), value.to_owned()))
                })
                .collect::<Option<Vec<_>>>()?;
            let argv = words[executable_index..].to_vec();
            let requested = argv.first()?.clone();
            let inline_path = assignments
                .iter()
                .rev()
                .find_map(|(name, value)| (name == "PATH").then_some(value.as_str()));
            let request_path = request_env
                .and_then(|env| env.get("PATH"))
                .and_then(Value::as_str);
            let inherited_path = std::env::var_os("PATH");
            let path = inline_path
                .map(std::ffi::OsString::from)
                .or_else(|| request_path.map(std::ffi::OsString::from))
                .or(inherited_path);
            Some(ExactShellCommand {
                operator_after,
                assignments,
                argv,
                executable: ExactExecutableIdentity {
                    requested: requested.clone(),
                    resolved: resolve_executable_identity(&requested, &cwd, path.as_deref()),
                },
            })
        })
        .collect()
}

pub(super) fn resolve_executable_identity(
    executable: &str,
    cwd: &Path,
    path: Option<&std::ffi::OsStr>,
) -> Option<ResolvedExecutableIdentity> {
    let executable_path = Path::new(executable);
    let candidates = if executable_path.components().count() > 1 || executable_path.is_absolute() {
        vec![if executable_path.is_absolute() {
            executable_path.to_path_buf()
        } else {
            cwd.join(executable_path)
        }]
    } else {
        path.map(std::env::split_paths)
            .into_iter()
            .flatten()
            .map(|directory| {
                if directory.is_absolute() {
                    directory.join(executable_path)
                } else {
                    cwd.join(directory).join(executable_path)
                }
            })
            .collect()
    };
    for candidate in candidates {
        let Ok(canonical) = fs::canonicalize(&candidate) else {
            continue;
        };
        if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let Ok(mut file) = fs::File::open(&canonical) else {
            continue;
        };
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = std::io::Read::read(&mut file, &mut buffer).ok()?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        return Some(ResolvedExecutableIdentity {
            canonical_path: canonical.as_os_str().as_encoded_bytes().to_vec(),
            content_hash: hasher.finalize().to_hex().to_string(),
            trusted_immutable: trusted_immutable_executable(&canonical),
        });
    }
    None
}

#[cfg(unix)]
pub(super) fn trusted_immutable_executable(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    fs::metadata(path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0
    })
}

#[cfg(not(unix))]
pub(super) fn trusted_immutable_executable(_path: &Path) -> bool {
    false
}

pub(super) fn rememberable_request(request: &PermissionRequest, behavior: ToolBehavior) -> bool {
    if behavior != ToolBehavior::Shell {
        return true;
    }
    let Some(command) = request.arguments.get("command").and_then(Value::as_str) else {
        return false;
    };
    if command.contains(['`', '$', '*', '?', '[', ']', '{', '}', '~', '\r']) {
        return false;
    }
    let Some(commands) = exact_shell_identity(command, &request.arguments) else {
        return false;
    };
    !commands.is_empty() && commands.iter().all(rememberable_shell_command)
}

pub(super) fn rememberable_shell_command(command: &ExactShellCommand) -> bool {
    if command.assignments.iter().any(|(name, _)| name == "PATH") {
        return false;
    }
    let executable = Path::new(&command.executable.requested)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(
        executable,
        "eval" | "cd" | "export" | "unset" | "source" | "." | "alias" | "unalias" | "set" | "exec"
    ) {
        return false;
    }
    let interpreter = matches!(
        executable,
        "sh" | "bash" | "zsh" | "dash" | "python" | "python3" | "node" | "ruby" | "perl"
    );
    if interpreter && command.argv.iter().skip(1).any(|argument| argument == "-c") {
        return false;
    }
    command
        .executable
        .resolved
        .as_ref()
        .is_some_and(|identity| identity.trusted_immutable)
}

pub(super) fn split_compound_with_operators(
    command: &str,
) -> Option<Vec<(String, Option<String>)>> {
    let chars = command.char_indices().collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let (offset, character) = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if character == '\\' && !single {
            escaped = true;
            index += 1;
            continue;
        }
        if character == '\'' && !double {
            single = !single;
            index += 1;
            continue;
        }
        if character == '"' && !single {
            double = !double;
            index += 1;
            continue;
        }
        if !single && !double {
            let next = chars.get(index + 1).map(|(_, next)| *next);
            let operator = match (character, next) {
                ('&', Some('&')) => Some(("&&", 2)),
                ('|', Some('|')) => Some(("||", 2)),
                (';', _) => Some((";", 1)),
                ('|', _) => Some(("|", 1)),
                ('\n', _) => Some(("\n", 1)),
                ('&' | '(' | ')' | '<' | '>', _) => return None,
                _ => None,
            };
            if let Some((operator, delimiter_len)) = operator {
                let segment = command.get(start..offset)?.trim();
                if segment.is_empty() {
                    return None;
                }
                segments.push((segment.to_owned(), Some(operator.to_owned())));
                index += delimiter_len;
                start = chars.get(index).map_or(command.len(), |(next, _)| *next);
                continue;
            }
        }
        index += 1;
    }
    if single || double || escaped {
        return None;
    }
    let tail = command.get(start..)?.trim();
    if tail.is_empty() {
        return None;
    }
    segments.push((tail.to_owned(), None));
    Some(segments)
}

pub(super) fn canonical_webfetch_origin(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https")
        .then(|| url.origin().ascii_serialization())
        .filter(|origin| origin != "null")
}

pub(super) fn normalize_network_domains(value: &Value) -> Option<Vec<String>> {
    let mut normalized = value
        .as_array()?
        .iter()
        .map(|domain| {
            let domain = domain
                .as_str()?
                .trim()
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if domain.is_empty()
                || domain.len() > 253
                || domain.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
            {
                None
            } else {
                Some(domain)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    normalized.sort();
    normalized.dedup();
    Some(normalized)
}

pub(super) fn is_read_only(request: &PermissionRequest, behavior: ToolBehavior) -> bool {
    (request.capabilities.is_empty()
        && matches!(
            behavior,
            ToolBehavior::UserInteraction | ToolBehavior::PlanSubmission
        ))
        || (!request.capabilities.is_empty()
            && request
                .capabilities
                .iter()
                .all(|capability| matches!(capability, ToolCapability::ReadFilesystem)))
}

pub(super) fn bash_sandbox_mode(request: &PermissionRequest) -> Option<BashSandboxMode> {
    match request.arguments.get("sandbox") {
        None => Some(BashSandboxMode::Sandboxed),
        Some(Value::String(mode)) if mode == "sandboxed" => Some(BashSandboxMode::Sandboxed),
        Some(Value::String(mode)) if mode == "unsandboxed" => Some(BashSandboxMode::Unsandboxed),
        Some(_) => None,
    }
}

pub(super) fn is_builtin_read_only_bash(
    request: &PermissionRequest,
    behavior: ToolBehavior,
) -> bool {
    behavior == ToolBehavior::Shell
        && bash_sandbox_mode(request) == Some(BashSandboxMode::Sandboxed)
        && request
            .arguments
            .get("network_domains")
            .is_none_or(|domains| {
                normalize_network_domains(domains).is_some_and(|domains| domains.is_empty())
            })
        && request
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| classify_safe_command(command) == CommandSafety::SafeListed)
}
