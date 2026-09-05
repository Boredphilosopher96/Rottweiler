//! Redaction policy for explicit, bounded history exports.
use rw_providers::FixtureRedactor;
use serde_json::Value;

pub(super) fn redact_export_value(value: Value, redactor: &FixtureRedactor) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| {
                    let lowered = key.to_ascii_lowercase();
                    let sensitive = [
                        "token",
                        "password",
                        "secret",
                        "api_key",
                        "authorization",
                        "credential",
                        "signature",
                    ]
                    .iter()
                    .any(|marker| lowered.contains(marker));
                    if sensitive {
                        (key, Value::String("[REDACTED]".to_owned()))
                    } else {
                        (key, redact_export_value(value, redactor))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_export_value(value, redactor))
                .collect(),
        ),
        Value::String(value) => Value::String(redact_export_string(&value, redactor)),
        other => other,
    }
}

fn redact_export_string(value: &str, redactor: &FixtureRedactor) -> String {
    let value = redactor.redact_text(value);
    let value = redact_embedded_paths(&value);
    let value = redact_embedded_known_secrets(&value);
    value
        .split_inclusive(char::is_whitespace)
        .map(|part| {
            let token = part.trim_end_matches(char::is_whitespace);
            let suffix = &part[token.len()..];
            if looks_like_secret(token) {
                format!("[REDACTED]{suffix}")
            } else {
                part.to_owned()
            }
        })
        .collect()
}

fn redact_embedded_paths(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let Some(start) = next_absolute_path_start(value, cursor) else {
            output.push_str(&value[cursor..]);
            break;
        };
        output.push_str(&value[cursor..start]);
        output.push_str("[REDACTED_PATH]");
        cursor = absolute_path_end(value, start);
    }
    output
}

fn next_absolute_path_start(value: &str, cursor: usize) -> Option<usize> {
    value[cursor..]
        .char_indices()
        .find_map(|(offset, character)| {
            let index = cursor + offset;
            if !path_has_left_boundary(value, index) {
                return None;
            }
            let tail = &value[index..];
            if character == '/' && is_known_slash_command(tail) {
                return None;
            }
            let file_uri = tail.starts_with("file:///") || tail.starts_with("file://localhost/");
            let html_closing_tag = character == '/'
                && value[..index].ends_with('<')
                && tail
                    .strip_prefix('/')
                    .and_then(|tag| tag.strip_suffix('>'))
                    .is_some_and(|tag| {
                        !tag.is_empty()
                            && tag.chars().all(|character| {
                                character.is_ascii_alphanumeric() || character == '-'
                            })
                    });
            let unix = character == '/'
                && !html_closing_tag
                && tail.as_bytes().get(1).is_some_and(|next| *next != b'/');
            let windows_drive = character.is_ascii_alphabetic()
                && tail.as_bytes().get(1) == Some(&b':')
                && tail
                    .as_bytes()
                    .get(2)
                    .is_some_and(|separator| matches!(separator, b'/' | b'\\'));
            let windows_unc = tail.starts_with("\\\\")
                && tail.as_bytes().get(2).is_some_and(|next| *next != b'\\');
            (file_uri || unix || windows_drive || windows_unc).then_some(index)
        })
}

fn is_known_slash_command(value: &str) -> bool {
    let token = value
        .strip_prefix('/')
        .and_then(|value| {
            let end = value
                .find(|character: char| {
                    character.is_whitespace()
                        || matches!(character, '"' | '\'' | '<' | '>' | ')' | ']' | '}' | ',')
                })
                .unwrap_or(value.len());
            value.get(..end)
        })
        .unwrap_or_default();
    matches!(
        token,
        "help"
            | "status"
            | "mode"
            | "permissions"
            | "plan"
            | "rewind"
            | "fork"
            | "review"
            | "interrupt"
            | "context"
            | "cost"
            | "compact"
            | "trust"
            | "add-dir"
            | "init"
            | "deep-init"
            | "memory"
            | "models"
            | "providers"
            | "mcp"
            | "mcp.prompt"
            | "project"
            | "workspace"
            | "session"
            | "plugin"
    )
}

fn path_has_left_boundary(value: &str, index: usize) -> bool {
    index == 0
        || value[..index].chars().next_back().is_some_and(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '=' | '"' | '\'' | '<' | '>' | '(' | '[' | '{' | ',' | ';'
                )
        })
}

fn absolute_path_end(value: &str, start: usize) -> usize {
    value[start..]
        .char_indices()
        .skip(1)
        .find(|(_, character)| {
            character.is_whitespace()
                || matches!(
                    character,
                    '"' | '\'' | '<' | '>' | ')' | ']' | '}' | ',' | ';'
                )
        })
        .map_or(value.len(), |(offset, _)| start + offset)
}

fn redact_embedded_known_secrets(value: &str) -> String {
    redact_matching_spans(
        value,
        &["github_pat_", "ghp_", "sk-", "AKIA"],
        "[REDACTED]",
        looks_like_secret,
    )
}

fn redact_matching_spans(
    value: &str,
    markers: &[&str],
    replacement: &str,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let next = markers
            .iter()
            .filter_map(|marker| {
                value[cursor..]
                    .find(marker)
                    .map(|offset| (cursor + offset, marker))
            })
            .min_by_key(|(offset, _)| *offset);
        let Some((start, marker)) = next else {
            output.push_str(&value[cursor..]);
            break;
        };
        output.push_str(&value[cursor..start]);
        let end = span_end(value, start + marker.len());
        let candidate = &value[start..end];
        if predicate(candidate) {
            output.push_str(replacement);
        } else {
            output.push_str(candidate);
        }
        cursor = end;
    }
    output
}

fn span_end(value: &str, after_marker: usize) -> usize {
    value[after_marker..]
        .char_indices()
        .find(|(_, character)| {
            character.is_whitespace()
                || matches!(
                    character,
                    '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                )
        })
        .map_or(value.len(), |(offset, _)| after_marker + offset)
}

fn looks_like_secret(value: &str) -> bool {
    let trimmed = value.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.')
    });
    if trimmed.starts_with("sk-")
        || trimmed.starts_with("ghp_")
        || trimmed.starts_with("github_pat_")
        || (trimmed.starts_with("AKIA") && trimmed.len() == 20)
    {
        return true;
    }
    if looks_like_utc_timestamp(trimmed) {
        return false;
    }
    if !(24..=4_096).contains(&trimmed.len())
        || trimmed.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return false;
    }
    let classes = [
        trimmed.bytes().any(|byte| byte.is_ascii_lowercase()),
        trimmed.bytes().any(|byte| byte.is_ascii_uppercase()),
        trimmed.bytes().any(|byte| byte.is_ascii_digit()),
        trimmed
            .bytes()
            .any(|byte| matches!(byte, b'-' | b'_' | b'+' | b'/' | b'=')),
    ];
    if classes.into_iter().filter(|present| *present).count() < 3 {
        return false;
    }
    shannon_entropy(trimmed.as_bytes()) >= 3.5
}

fn looks_like_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    (20..=35).contains(&bytes.len())
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
        && bytes.get(16) == Some(&b':')
        && bytes.last() == Some(&b'Z')
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16)
                || *byte == b'.'
                || byte.is_ascii_digit()
                || (index + 1 == bytes.len() && *byte == b'Z')
        })
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    let mut counts = [0_u32; 256];
    for byte in bytes {
        counts[usize::from(*byte)] += 1;
    }
    let length = u32::try_from(bytes.len()).map_or(f64::from(u32::MAX), f64::from);
    counts
        .into_iter()
        .filter(|count| *count != 0)
        .map(|count| {
            let probability = f64::from(count) / length;
            -probability * probability.log2()
        })
        .sum()
}

#[cfg(test)]
#[path = "redaction_tests.rs"]
mod tests;
