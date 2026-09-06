use super::{ImportItem, ImportStatus, item, safe_name};
use miette::{Result, miette};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default, Serialize)]
struct McpTarget {
    servers: BTreeMap<String, McpServer>,
}

#[derive(Serialize)]
struct McpServer {
    enabled: bool,
    defer_tools: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    argv: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inherit_env: Vec<String>,
}

pub(super) fn render_claude_mcp(
    value: &Value,
    diagnostics: &mut Vec<ImportItem>,
) -> Result<Option<String>> {
    render_mcp_map(value.get("mcpServers"), false, diagnostics)
}

pub(super) fn render_opencode_mcp(
    value: &Value,
    diagnostics: &mut Vec<ImportItem>,
) -> Result<Option<String>> {
    render_mcp_map(value.get("mcp"), true, diagnostics)
}

#[allow(clippy::too_many_lines)]
pub(super) fn render_mcp_map(
    value: Option<&Value>,
    opencode: bool,
    diagnostics: &mut Vec<ImportItem>,
) -> Result<Option<String>> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut target = McpTarget::default();
    for (name, entry) in map {
        if !safe_name(name) {
            diagnostics.push(item(
                "mcp",
                name,
                ImportStatus::Unsupported,
                "unsafe server name",
            ));
            continue;
        }
        if entry.get("headers").is_some()
            || entry.get("oauth").is_some()
            || entry.get("authorization").is_some()
        {
            diagnostics.push(item(
                "mcp",
                name,
                ImportStatus::Unsupported,
                "authenticated remote MCP configuration is not imported; configure Rottweiler credential references or OAuth explicitly",
            ));
            continue;
        }
        let endpoint = entry.get("url").and_then(Value::as_str).map(str::to_owned);
        let argv: Option<Vec<String>> = if opencode {
            entry
                .get("command")
                .and_then(Value::as_array)
                .filter(|values| values.iter().all(Value::is_string))
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
        } else {
            entry.get("command").and_then(Value::as_str).map(|command| {
                std::iter::once(command.to_owned())
                    .chain(
                        entry
                            .get("args")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned),
                    )
                    .collect()
            })
        };
        let argv = argv.filter(|argv| {
            !argv.is_empty()
                && argv
                    .iter()
                    .all(|value| !value.is_empty() && !value.contains('\0'))
                && argv
                    .first()
                    .is_some_and(|program| Path::new(program).is_absolute())
        });
        let endpoint = endpoint.filter(|endpoint| secure_remote_endpoint(endpoint));
        let declared_remote = entry.get("url").is_some();
        let declared_stdio = entry.get("command").is_some();
        if declared_remote && endpoint.is_none() {
            diagnostics.push(item(
                "mcp",
                name,
                ImportStatus::Unsupported,
                "only credential-free HTTPS remote MCP endpoints are imported",
            ));
            continue;
        }
        if declared_stdio && argv.is_none() {
            diagnostics.push(item(
                "mcp",
                name,
                ImportStatus::Unsupported,
                "stdio command must use an absolute executable",
            ));
            continue;
        }
        if endpoint.is_some() == argv.is_some() {
            diagnostics.push(item(
                "mcp",
                name,
                ImportStatus::Unsupported,
                "MCP server must declare exactly one supported remote or stdio transport",
            ));
            continue;
        }
        let env = entry
            .get(if opencode { "environment" } else { "env" })
            .and_then(Value::as_object)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|(key, value)| {
                        let expected = if opencode {
                            format!("{{env:{key}}}")
                        } else {
                            format!("${{{key}}}")
                        };
                        (value.as_str() == Some(expected.as_str()) && safe_inherited_env(key))
                            .then(|| key.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if entry
            .get(if opencode { "environment" } else { "env" })
            .and_then(Value::as_object)
            .is_some_and(|values| values.len() != env.len())
        {
            diagnostics.push(item(
                "credential",
                name,
                ImportStatus::Unsupported,
                "literal or security-sensitive MCP environment values are never imported; configure credential references manually",
            ));
        }
        target.servers.insert(
            name.clone(),
            McpServer {
                enabled: entry
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                defer_tools: true,
                argv,
                endpoint,
                inherit_env: env,
            },
        );
    }
    if target.servers.is_empty() {
        Ok(None)
    } else {
        toml::to_string_pretty(&target)
            .map(Some)
            .map_err(|error| miette!("MCP import could not render: {error}"))
    }
}

pub(super) fn secure_remote_endpoint(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

pub(super) fn safe_inherited_env(value: &str) -> bool {
    matches!(
        value,
        "PATH"
            | "HOME"
            | "TMPDIR"
            | "LANG"
            | "LC_ALL"
            | "LC_CTYPE"
            | "TERM"
            | "COLORTERM"
            | "NO_COLOR"
    )
}

#[derive(Serialize)]
struct HookTarget {
    #[serde(rename = "hook")]
    hooks: Vec<HookEntry>,
}

#[derive(Serialize)]
struct HookEntry {
    id: String,
    event: String,
    matcher: String,
    run: String,
    class: rw_types::hook_contract::HookClass,
    failure_policy: rw_types::hook_contract::HookFailurePolicy,
}

pub(super) fn render_claude_hooks(
    value: &Value,
    diagnostics: &mut Vec<ImportItem>,
) -> Result<Option<String>> {
    let Some(hooks) = value.get("hooks").and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut output = Vec::new();
    for (source_event, groups) in hooks {
        let event = match source_event.as_str() {
            "PreToolUse" => "pre_tool",
            "PostToolUse" => "post_tool",
            _ => {
                diagnostics.push(item(
                    "hook",
                    source_event,
                    ImportStatus::Unsupported,
                    "only PreToolUse and PostToolUse command hooks have checkpoint-owned workspace effects",
                ));
                continue;
            }
        };
        for (group_index, group) in groups.as_array().into_iter().flatten().enumerate() {
            let matcher = group.get("matcher").and_then(Value::as_str).unwrap_or("*");
            for (hook_index, hook) in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if hook.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    continue;
                };
                if command.contains(['\r', '\n']) {
                    diagnostics.push(item(
                        "hook",
                        source_event,
                        ImportStatus::Unsupported,
                        "multiline hook command rejected",
                    ));
                    continue;
                }
                let Some(matchers) = claude_matchers(matcher) else {
                    diagnostics.push(item(
                        "hook",
                        source_event,
                        ImportStatus::Unsupported,
                        "Claude hook matcher uses unsupported regular-expression syntax",
                    ));
                    continue;
                };
                for (matcher_index, matcher) in matchers.into_iter().enumerate() {
                    output.push(HookEntry {
                        id: format!(
                            "import-claude-{event}-{group_index}-{hook_index}-{matcher_index}"
                        ),
                        event: event.to_owned(),
                        matcher,
                        run: format!("{{ {command}; }}; s=$?; [ \"$s\" -eq 2 ] && exit 1; exit 0"),
                        class: if event == "pre_tool" {
                            rw_types::hook_contract::HookClass::Policy
                        } else {
                            rw_types::hook_contract::HookClass::Transform
                        },
                        failure_policy: if event == "pre_tool" {
                            rw_types::hook_contract::HookFailurePolicy::FailClosed
                        } else {
                            rw_types::hook_contract::HookFailurePolicy::FailOpen
                        },
                    });
                }
            }
        }
    }
    if output.is_empty() {
        Ok(None)
    } else {
        toml::to_string_pretty(&HookTarget { hooks: output })
            .map(Some)
            .map_err(|error| miette!("hook import could not render: {error}"))
    }
}

pub(super) fn claude_matchers(value: &str) -> Option<Vec<String>> {
    if value == "*" || value.is_empty() {
        return Some(vec!["*".to_owned()]);
    }
    value
        .split('|')
        .map(|name| {
            (!name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
            .then(|| {
                let normalized = name
                    .chars()
                    .map(|character| match character {
                        '-' => '_',
                        character => character.to_ascii_lowercase(),
                    })
                    .collect::<String>();
                format!("{normalized}(*)")
            })
        })
        .collect()
}

pub(super) fn shift_claude_args(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit) {
            let start = index + 1;
            let mut end = start;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            let number = value[start..end].parse::<u32>().unwrap_or(u32::MAX);
            output.push('$');
            output.push_str(&number.saturating_add(1).to_string());
            index = end;
        } else {
            let character = value[index..].chars().next().unwrap_or_default();
            output.push(character);
            index += character.len_utf8();
        }
    }
    output
}

pub(super) fn strip_jsonc(bytes: &[u8]) -> Result<String> {
    let source = std::str::from_utf8(bytes).map_err(|_| miette!("JSONC is not UTF-8"))?;
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut quoted = false;
    let mut escaped = false;
    while let Some(character) = chars.next() {
        if quoted {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else if character == '"' {
            quoted = true;
            output.push('"');
        } else if character == '/' && chars.peek() == Some(&'/') {
            let _ = chars.next();
            output.push(' ');
            for comment in chars.by_ref() {
                if comment == '\n' {
                    output.push('\n');
                    break;
                }
            }
        } else if character == '/' && chars.peek() == Some(&'*') {
            let _ = chars.next();
            output.push(' ');
            let mut closed = false;
            while let Some(comment) = chars.next() {
                if comment == '\n' {
                    output.push('\n');
                } else if comment == '*' && chars.peek() == Some(&'/') {
                    let _ = chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err(miette!("JSONC block comment is unterminated"));
            }
        } else {
            output.push(character);
        }
    }
    let chars = output.chars().collect::<Vec<_>>();
    let mut cleaned = String::with_capacity(output.len());
    quoted = false;
    escaped = false;
    for (index, character) in chars.iter().copied().enumerate() {
        if quoted {
            cleaned.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            continue;
        }
        if character == '"' {
            quoted = true;
            cleaned.push(character);
            continue;
        }
        if character == ','
            && chars[index + 1..]
                .iter()
                .copied()
                .find(|next| !next.is_whitespace())
                .is_some_and(|next| matches!(next, '}' | ']'))
        {
            continue;
        }
        cleaned.push(character);
    }
    Ok(cleaned)
}

pub(super) fn parse_json(bytes: &[u8], kind: &str) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| miette!("{kind} is malformed"))
}
