use std::{path::Path, time::Duration};

use toml::{Table, Value};

use crate::{HookEffect, HookEvent, HookFailurePolicy, HookRegistration};
use rw_types::ToolCapability;

use super::{
    ArtifactLocation, ArtifactOrigin, ArtifactScope, ExtensionDiscoveryError, read_bounded_utf8,
};

const MAX_HOOKS_TOML_BYTES: u64 = 1024 * 1024;
const MIN_HOOK_TIMEOUT_MS: u64 = 50;
const MAX_HOOK_TIMEOUT_MS: u64 = 120_000;

#[derive(Clone, Debug, Eq, PartialEq)]
struct LazyHookCommand {
    path: std::path::PathBuf,
    digest: blake3::Hash,
    command: String,
}

impl LazyHookCommand {
    fn load(&self) -> Result<String, ExtensionDiscoveryError> {
        let contents = read_bounded_utf8(&self.path, MAX_HOOKS_TOML_BYTES)?;
        if blake3::hash(contents.as_bytes()) != self.digest {
            return Err(ExtensionDiscoveryError::ChangedAfterDiscovery {
                path: self.path.clone(),
            });
        }
        Ok(self.command.clone())
    }
}

/// Declarative `hooks.toml` entry. The command remains lazy and is never
/// executed by discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredShellHook {
    registration: HookRegistration,
    matcher: String,
    origin: ArtifactOrigin,
    command: LazyHookCommand,
}

impl DiscoveredShellHook {
    #[must_use]
    pub fn id(&self) -> &str {
        self.registration.id()
    }

    #[must_use]
    pub const fn registration(&self) -> &HookRegistration {
        &self.registration
    }

    #[must_use]
    pub fn matcher(&self) -> &str {
        &self.matcher
    }

    #[must_use]
    pub const fn origin(&self) -> &ArtifactOrigin {
        &self.origin
    }

    /// Loads the one-line command only after runtime composition has applied
    /// folder trust. This method returns data; it never starts a process.
    ///
    /// # Errors
    ///
    /// Fails closed if `hooks.toml` changed since discovery.
    pub fn load_command(&self) -> Result<String, ExtensionDiscoveryError> {
        self.command.load()
    }
}

pub(super) fn discover_file(
    scope: ArtifactScope,
    location: ArtifactLocation,
    path: &Path,
) -> Result<Vec<DiscoveredShellHook>, ExtensionDiscoveryError> {
    let contents = read_bounded_utf8(path, MAX_HOOKS_TOML_BYTES)?;
    let digest = blake3::hash(contents.as_bytes());
    let root = toml::from_str::<Table>(&contents).map_err(|source| {
        ExtensionDiscoveryError::InvalidHooksToml {
            path: path.to_owned(),
            message: source.message().to_owned(),
        }
    })?;
    if root.keys().any(|key| key != "hook") {
        return Err(ExtensionDiscoveryError::InvalidHooksToml {
            path: path.to_owned(),
            message: "only `[[hook]]` entries are supported".to_owned(),
        });
    }
    let Some(entries) = root.get("hook") else {
        return Ok(Vec::new());
    };
    let entries = entries
        .as_array()
        .ok_or_else(|| ExtensionDiscoveryError::InvalidHooksToml {
            path: path.to_owned(),
            message: "`hook` must be an array of tables".to_owned(),
        })?;
    entries
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let table = value
                .as_table()
                .ok_or_else(|| invalid_hook(path, index, "each `hook` entry must be a table"))?;
            parse_hook(scope, location, path, digest, index, table)
        })
        .collect()
}

fn parse_hook(
    scope: ArtifactScope,
    location: ArtifactLocation,
    path: &Path,
    digest: blake3::Hash,
    index: usize,
    table: &Table,
) -> Result<DiscoveredShellHook, ExtensionDiscoveryError> {
    const ALLOWED_FIELDS: &[&str] = &[
        "id",
        "event",
        "matcher",
        "run",
        "priority",
        "timeout_ms",
        "failure_policy",
        "failure-policy",
        "effect",
    ];
    if let Some(unknown) = table
        .keys()
        .find(|key| !ALLOWED_FIELDS.contains(&key.as_str()))
    {
        return Err(invalid_hook(
            path,
            index,
            &format!("unsupported field `{unknown}`"),
        ));
    }
    let event_name = required_string(path, index, table, "event")?;
    let event = parse_event(path, index, event_name)?;
    let matcher = required_string(path, index, table, "matcher")?.trim();
    if matcher.is_empty() {
        return Err(invalid_hook(path, index, "`matcher` must not be empty"));
    }
    let command = required_string(path, index, table, "run")?;
    let command = command.trim();
    if command.is_empty() || command.contains(['\r', '\n']) {
        return Err(invalid_hook(
            path,
            index,
            "`run` must be a non-empty one-line command",
        ));
    }
    let id = table.get("id").map_or_else(
        || Ok(generated_id(scope, location, index)),
        |value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_hook(path, index, "`id` must be a string"))
        },
    )?;
    if id.is_empty() || id.chars().any(char::is_control) {
        return Err(invalid_hook(
            path,
            index,
            "`id` must not be empty or contain control characters",
        ));
    }
    let priority = optional_i32(path, index, table, "priority")?.unwrap_or(0);
    let timeout_ms = optional_positive_u64(path, index, table, "timeout_ms")?.unwrap_or(5_000);
    if !(MIN_HOOK_TIMEOUT_MS..=MAX_HOOK_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(invalid_hook(
            path,
            index,
            &format!(
                "`timeout_ms` must be between {MIN_HOOK_TIMEOUT_MS} and {MAX_HOOK_TIMEOUT_MS}"
            ),
        ));
    }
    let failure_policy = parse_failure_policy(path, index, table)?;
    let effect = parse_effect(path, index, table)?;
    let applicable_tools = matcher
        .split_once('(')
        .and_then(|(name, pattern)| pattern.ends_with(')').then_some(name))
        .into_iter()
        .map(str::to_owned);
    let registration = HookRegistration::new(id, event)
        .with_priority(priority)
        .with_timeout(Duration::from_millis(timeout_ms))
        .with_failure_policy(failure_policy)
        .with_effect(effect)
        .with_applicable_tools(applicable_tools)
        .with_required_capabilities([ToolCapability::Execute]);
    Ok(DiscoveredShellHook {
        registration,
        matcher: matcher.to_owned(),
        origin: ArtifactOrigin {
            scope,
            location,
            path: path.to_owned(),
        },
        command: LazyHookCommand {
            path: path.to_owned(),
            digest,
            command: command.to_owned(),
        },
    })
}

fn parse_effect(
    path: &Path,
    index: usize,
    table: &Table,
) -> Result<HookEffect, ExtensionDiscoveryError> {
    match table.get("effect") {
        None => Ok(HookEffect::WorkspaceMutating),
        Some(Value::String(value)) if value == "workspace-mutating" => {
            Ok(HookEffect::WorkspaceMutating)
        }
        Some(Value::String(value)) if value == "read-only" => Ok(HookEffect::ReadOnly),
        Some(_) => Err(invalid_hook(
            path,
            index,
            "`effect` must be `read-only` or `workspace-mutating`",
        )),
    }
}

fn generated_id(scope: ArtifactScope, location: ArtifactLocation, index: usize) -> String {
    let scope = match scope {
        ArtifactScope::Project => "project",
        ArtifactScope::User => "user",
    };
    let location = match location {
        ArtifactLocation::Agents => "agents",
        ArtifactLocation::Rottweiler => "rottweiler",
    };
    format!("shell.{scope}.{location}.{}", index + 1)
}

fn parse_event(
    path: &Path,
    index: usize,
    event: &str,
) -> Result<HookEvent, ExtensionDiscoveryError> {
    match event {
        "session_start" => Ok(HookEvent::SessionStart),
        "session_end" => Ok(HookEvent::SessionEnd),
        "user_prompt_submit" => Ok(HookEvent::UserPromptSubmit),
        "pre_tool" => Ok(HookEvent::PreTool),
        "post_tool" => Ok(HookEvent::PostTool),
        "pre_compact" => Ok(HookEvent::PreCompact),
        "turn_end" => Ok(HookEvent::TurnEnd),
        "permission_check" => Ok(HookEvent::PermissionCheck),
        _ => Err(invalid_hook(
            path,
            index,
            &format!("unsupported hook event `{event}`"),
        )),
    }
}

fn parse_failure_policy(
    path: &Path,
    index: usize,
    table: &Table,
) -> Result<HookFailurePolicy, ExtensionDiscoveryError> {
    if table.contains_key("failure_policy") && table.contains_key("failure-policy") {
        return Err(invalid_hook(
            path,
            index,
            "use only one of `failure_policy` or `failure-policy`",
        ));
    }
    let value = table
        .get("failure_policy")
        .or_else(|| table.get("failure-policy"));
    match value {
        None => Ok(HookFailurePolicy::FailOpen),
        Some(Value::String(value)) if value == "fail-open" => Ok(HookFailurePolicy::FailOpen),
        Some(Value::String(value)) if value == "fail-closed" => Ok(HookFailurePolicy::FailClosed),
        Some(_) => Err(invalid_hook(
            path,
            index,
            "failure policy must be `fail-open` or `fail-closed`",
        )),
    }
}

fn required_string<'a>(
    path: &Path,
    index: usize,
    table: &'a Table,
    field: &str,
) -> Result<&'a str, ExtensionDiscoveryError> {
    table
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_hook(path, index, &format!("`{field}` must be a string")))
}

fn optional_i32(
    path: &Path,
    index: usize,
    table: &Table,
    field: &str,
) -> Result<Option<i32>, ExtensionDiscoveryError> {
    table
        .get(field)
        .map(|value| {
            value
                .as_integer()
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| invalid_hook(path, index, &format!("`{field}` must fit an i32")))
        })
        .transpose()
}

fn optional_positive_u64(
    path: &Path,
    index: usize,
    table: &Table,
    field: &str,
) -> Result<Option<u64>, ExtensionDiscoveryError> {
    table
        .get(field)
        .map(|value| {
            value
                .as_integer()
                .and_then(|value| u64::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    invalid_hook(
                        path,
                        index,
                        &format!("`{field}` must be a positive integer"),
                    )
                })
        })
        .transpose()
}

fn invalid_hook(path: &Path, zero_based_index: usize, message: &str) -> ExtensionDiscoveryError {
    ExtensionDiscoveryError::InvalidHook {
        path: path.to_owned(),
        index: zero_based_index + 1,
        message: message.to_owned(),
    }
}
