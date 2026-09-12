use std::{collections::BTreeSet, path::Path};

use globset::GlobBuilder;
use rw_tools::{BashSandboxMode, ToolBehavior};
use rw_types::config::{PermissionConfig, PermissionDecision};
use serde_json::Value;

use super::{PermissionRequest, bash_sandbox_mode, normalize_network_domains};

pub(super) fn rule_decision(
    config: &PermissionConfig,
    request: &PermissionRequest,
    behavior: ToolBehavior,
) -> PermissionDecision {
    let Some(targets) = canonical_arguments_for(request, behavior) else {
        return config.default;
    };
    let mut all_allowed = !targets.is_empty();
    let mut any_asked = false;
    for target in targets {
        let mut target_decision = None;
        for rule in &config.rules {
            let Some((tool, pattern)) = parse_rule(&rule.pattern) else {
                continue;
            };
            if tool != request.tool_name || !glob_matches(pattern, &target) {
                continue;
            }
            if rule.action == PermissionDecision::Deny {
                return PermissionDecision::Deny;
            }
            target_decision = Some(rule.action);
        }
        if target_decision == Some(PermissionDecision::Ask) {
            any_asked = true;
        }
        if target_decision != Some(PermissionDecision::Allow) {
            all_allowed = false;
        }
    }
    if any_asked {
        PermissionDecision::Ask
    } else if all_allowed {
        if request
            .arguments
            .get("network_domains")
            .and_then(normalize_network_domains)
            .is_some_and(|domains| !domains.is_empty())
        {
            capability_rule_decision(config, "network", &request.tool_name)
                .unwrap_or(config.default)
        } else {
            PermissionDecision::Allow
        }
    } else {
        config.default
    }
}

/// Returns authority from the explicit `bash_unsandboxed(pattern)` namespace.
/// Ordinary `bash(pattern)` allows never imply permission to bypass the native
/// sandbox; their deny decisions are still honored by `decision_for`.
pub(super) fn unsandboxed_rule_decision(
    config: &PermissionConfig,
    request: &PermissionRequest,
    behavior: ToolBehavior,
) -> Option<PermissionDecision> {
    if behavior != ToolBehavior::Shell
        || bash_sandbox_mode(request) != Some(BashSandboxMode::Unsandboxed)
    {
        return None;
    }
    let targets = canonical_arguments_for(request, behavior)?;
    let mut all_allowed = !targets.is_empty();
    let mut any_asked = false;
    let mut any_matched = false;
    for target in targets {
        let mut target_decision = None;
        for rule in &config.rules {
            let Some((tool, pattern)) = parse_rule(&rule.pattern) else {
                continue;
            };
            if tool != "bash_unsandboxed" || !glob_matches(pattern, &target) {
                continue;
            }
            any_matched = true;
            if rule.action == PermissionDecision::Deny {
                return Some(PermissionDecision::Deny);
            }
            target_decision = Some(rule.action);
        }
        any_asked |= target_decision == Some(PermissionDecision::Ask);
        all_allowed &= target_decision == Some(PermissionDecision::Allow);
    }
    if any_asked {
        Some(PermissionDecision::Ask)
    } else if all_allowed && any_matched {
        Some(PermissionDecision::Allow)
    } else {
        None
    }
}

pub(super) fn validate_rule(rule: &str) -> Result<(), String> {
    let Some((tool, pattern)) = parse_rule(rule) else {
        return Err("permission rule must use tool(glob) syntax".to_owned());
    };
    if !tool
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err("permission rule tool names use letters, digits, `_`, or `-`".to_owned());
    }
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .map(|_| ())
        .map_err(|error| format!("invalid permission glob: {error}"))
}

pub(super) fn capability_rule_decision(
    config: &PermissionConfig,
    capability: &str,
    tool_name: &str,
) -> Option<PermissionDecision> {
    let mut decision = None;
    for rule in &config.rules {
        let Some((tool, pattern)) = parse_rule(&rule.pattern) else {
            continue;
        };
        if tool != capability || !glob_matches(pattern, tool_name) {
            continue;
        }
        if rule.action == PermissionDecision::Deny {
            return Some(PermissionDecision::Deny);
        }
        decision = Some(rule.action);
    }
    decision
}

pub(super) fn parse_rule(rule: &str) -> Option<(&str, &str)> {
    let open = rule.find('(')?;
    let tool = rule[..open].trim();
    let pattern = rule.get(open + 1..rule.len().checked_sub(1)?)?;
    (!tool.is_empty() && rule.ends_with(')')).then_some((tool, pattern))
}

pub(super) fn glob_matches(pattern: &str, target: &str) -> bool {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(target))
}

pub(super) fn canonical_arguments_for(
    request: &PermissionRequest,
    behavior: ToolBehavior,
) -> Option<Vec<String>> {
    if behavior == ToolBehavior::Shell {
        return request
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .and_then(canonical_shell_commands);
    }
    for key in ["path", "url", "domain", "command"] {
        if let Some(value) = request.arguments.get(key).and_then(Value::as_str) {
            return Some(vec![value.trim().to_owned()]);
        }
    }
    Some(vec![canonical_json(&request.arguments)])
}

pub(super) fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

pub(super) fn canonical_shell_commands(command: &str) -> Option<Vec<String>> {
    // Permission allow rules must bind the argv the process will actually
    // receive. Shell expansion happens after tokenization, so unresolved
    // variables, globs, braces, and tildes fall back to the configured default
    // instead of matching an allow rule over misleading literal text.
    if command.contains(['`', '$', '*', '?', '[', ']', '{', '}', '~']) {
        return None;
    }
    let segments = split_compound(command)?;
    let mut canonical = Vec::with_capacity(segments.len());
    for segment in segments {
        let mut argv = shell_words::split(segment.trim()).ok()?;
        if argv.is_empty() {
            return None;
        }
        let command_index = argv.iter().position(|argument| !is_assignment(argument))?;
        if command_index != 0 {
            return None;
        }
        let binary = Path::new(argv.first()?).file_name()?.to_str()?.to_owned();
        if binary == "eval"
            || (["bash", "sh", "zsh", "dash"].contains(&binary.as_str())
                && argv.iter().skip(1).any(|argument| argument == "-c"))
        {
            return None;
        }
        argv[0] = binary;
        if argv[0] == "rm" {
            normalize_rm_flags(&mut argv);
        }
        canonical.push(argv.join(" "));
    }
    (!canonical.is_empty()).then_some(canonical)
}

pub(super) fn is_assignment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

pub(super) fn normalize_rm_flags(argv: &mut Vec<String>) {
    let option_end = argv
        .iter()
        .skip(1)
        .position(|argument| !argument.starts_with('-') || argument == "-")
        .map_or(argv.len(), |index| index + 1);
    let mut flags = BTreeSet::new();
    let mut long = Vec::new();
    for option in argv.drain(1..option_end) {
        if option.starts_with("--") {
            long.push(option);
        } else {
            flags.extend(option.trim_start_matches('-').chars());
        }
    }
    let mut normalized = String::from("-");
    for preferred in ['r', 'f'] {
        if flags.remove(&preferred) {
            normalized.push(preferred);
        }
    }
    normalized.extend(flags);
    let mut insertion = Vec::new();
    if normalized.len() > 1 {
        insertion.push(normalized);
    }
    long.sort();
    insertion.extend(long);
    argv.splice(1..1, insertion);
}

pub(super) fn split_compound(command: &str) -> Option<Vec<String>> {
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
            let next = chars.get(index + 1).map(|(_, c)| *c);
            let delimiter_len = match (character, next) {
                ('&', Some('&')) | ('|', Some('|')) => 2,
                (';' | '|' | '\n', _) => 1,
                ('&' | '(' | ')' | '<' | '>', _) => return None,
                _ => 0,
            };
            if delimiter_len > 0 {
                let segment = command.get(start..offset)?.trim();
                if segment.is_empty() {
                    return None;
                }
                segments.push(segment.to_owned());
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
    if !tail.is_empty() {
        segments.push(tail.to_owned());
    }
    Some(segments)
}
