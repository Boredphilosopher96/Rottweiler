use super::{
    ArtifactLocation, ArtifactOrigin, ArtifactScope, BTreeMap, CommandTemplate, DiscoveredAgent,
    DiscoveredCommand, DiscoveredSkill, ExtensionDiscoveryError, LazyMarkdownBody,
    MAX_MARKDOWN_BYTES, Path, TemplatePart, deduplicate, file_stem, invalid_frontmatter,
    read_bounded_relative_utf8, read_bounded_utf8, valid_frontmatter_key, validate_artifact_name,
    validate_mcp_virtual_tool,
};

#[derive(Debug)]
pub(super) struct FrontmatterDocument<'a> {
    fields: BTreeMap<String, FrontmatterValue>,
    pub(super) body: &'a str,
}

#[derive(Debug)]
pub(super) enum FrontmatterValue {
    Scalar(String),
    List(Vec<String>),
}

pub(super) fn discover_command(
    scope: ArtifactScope,
    location: ArtifactLocation,
    root: &Path,
    path: &Path,
) -> Result<DiscoveredCommand, ExtensionDiscoveryError> {
    let contents = read_bounded_utf8(path, MAX_MARKDOWN_BYTES)?;
    let digest = blake3::hash(contents.as_bytes());
    let document = parse_frontmatter(path, &contents)?;
    let name = file_stem(path)?;
    validate_artifact_name(path, &name)?;
    let description = required_scalar(path, &document.fields, "description")?;
    let model = optional_scalar(path, &document.fields, "model")?;
    let allowed_tools = optional_list(&document.fields, "allowed-tools");
    let argument_hint = optional_scalar(path, &document.fields, "argument-hint")?;
    Ok(DiscoveredCommand {
        name,
        description,
        model,
        allowed_tools,
        argument_hint,
        origin: ArtifactOrigin {
            scope,
            location,
            path: path.to_owned(),
        },
        body: LazyMarkdownBody {
            path: path.to_owned(),
            root: root.to_owned(),
            relative: path
                .strip_prefix(root)
                .map_err(|_| ExtensionDiscoveryError::InvalidPath {
                    path: path.to_owned(),
                })?
                .to_owned(),
            digest,
        },
    })
}

pub(super) fn discover_skill(
    scope: ArtifactScope,
    location: ArtifactLocation,
    source_root: &Path,
    path: &Path,
) -> Result<DiscoveredSkill, ExtensionDiscoveryError> {
    let contents = read_bounded_utf8(path, MAX_MARKDOWN_BYTES)?;
    let digest = blake3::hash(contents.as_bytes());
    let document = parse_frontmatter(path, &contents)?;
    let name = required_scalar(path, &document.fields, "name")?;
    validate_artifact_name(path, &name)?;
    let description = required_scalar(path, &document.fields, "description")?;
    let allowed_tools = optional_list(&document.fields, "allowed-tools");
    let root = path
        .parent()
        .ok_or_else(|| ExtensionDiscoveryError::InvalidPath {
            path: path.to_owned(),
        })?
        .to_owned();
    Ok(DiscoveredSkill {
        name,
        description,
        allowed_tools,
        origin: ArtifactOrigin {
            scope,
            location,
            path: path.to_owned(),
        },
        root,
        body: LazyMarkdownBody {
            path: path.to_owned(),
            root: source_root.to_owned(),
            relative: path
                .strip_prefix(source_root)
                .map_err(|_| ExtensionDiscoveryError::InvalidPath {
                    path: path.to_owned(),
                })?
                .to_owned(),
            digest,
        },
    })
}

pub(super) fn discover_agent(
    scope: ArtifactScope,
    location: ArtifactLocation,
    root: &Path,
    path: &Path,
) -> Result<DiscoveredAgent, ExtensionDiscoveryError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ExtensionDiscoveryError::InvalidPath {
            path: path.to_owned(),
        })?;
    let contents = read_bounded_relative_utf8(root, relative, MAX_MARKDOWN_BYTES)?;
    let digest = blake3::hash(contents.as_bytes());
    let document = parse_frontmatter(path, &contents)?;
    let name = required_scalar(path, &document.fields, "name")?;
    validate_artifact_name(path, &name)?;
    if file_stem(path)? != name {
        return Err(ExtensionDiscoveryError::InvalidAgent {
            path: path.to_owned(),
            message: "frontmatter `name` must match the file name".to_owned(),
        });
    }
    let description = required_scalar(path, &document.fields, "description")?;
    let model = required_scalar(path, &document.fields, "model")?;
    validate_artifact_name(path, &model)?;
    let tools = optional_list(&document.fields, "tools");
    if tools.len() > 128 {
        return Err(ExtensionDiscoveryError::InvalidAgent {
            path: path.to_owned(),
            message: "`tools` exceeds the 128-entry limit".to_owned(),
        });
    }
    if tools.iter().any(|tool| {
        let canonical = !tool.is_empty()
            && tool
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        !canonical && validate_mcp_virtual_tool(tool).is_err()
    }) {
        return Err(ExtensionDiscoveryError::InvalidAgent {
            path: path.to_owned(),
            message:
                "`tools` entries must be canonical tool names or exact mcp:<server>/<tool> grants"
                    .to_owned(),
        });
    }
    let permission_mode = required_scalar(path, &document.fields, "permission-mode")?
        .parse()
        .map_err(|_| ExtensionDiscoveryError::InvalidAgent {
            path: path.to_owned(),
            message: "`permission-mode` must be discuss, plan, or execute".to_owned(),
        })?;
    let max_turns = optional_scalar(path, &document.fields, "max-turns")?
        .map_or(Ok(32_usize), |value| value.parse::<usize>())
        .map_err(|_| ExtensionDiscoveryError::InvalidAgent {
            path: path.to_owned(),
            message: "`max-turns` must be an integer".to_owned(),
        })?;
    if !(1..=256).contains(&max_turns) {
        return Err(ExtensionDiscoveryError::InvalidAgent {
            path: path.to_owned(),
            message: "`max-turns` must be between 1 and 256".to_owned(),
        });
    }
    if document.body.trim().is_empty() {
        return Err(ExtensionDiscoveryError::InvalidAgent {
            path: path.to_owned(),
            message: "system prompt body must not be empty".to_owned(),
        });
    }
    Ok(DiscoveredAgent {
        name,
        description,
        model,
        tools,
        permission_mode,
        max_turns,
        origin: ArtifactOrigin {
            scope,
            location,
            path: path.to_owned(),
        },
        body: LazyMarkdownBody {
            path: path.to_owned(),
            root: root.to_owned(),
            relative: relative.to_owned(),
            digest,
        },
    })
}

pub(super) fn parse_frontmatter<'a>(
    path: &Path,
    contents: &'a str,
) -> Result<FrontmatterDocument<'a>, ExtensionDiscoveryError> {
    let normalized = contents.strip_prefix("\u{feff}").unwrap_or(contents);
    let mut offset = 0;
    let mut lines = normalized.split_inclusive('\n');
    let first = lines
        .next()
        .ok_or_else(|| ExtensionDiscoveryError::MissingFrontmatter {
            path: path.to_owned(),
        })?;
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Err(ExtensionDiscoveryError::MissingFrontmatter {
            path: path.to_owned(),
        });
    }
    offset += first.len();
    let mut frontmatter_lines = Vec::new();
    let mut closed = false;
    for (index, line) in lines.enumerate() {
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            closed = true;
            break;
        }
        frontmatter_lines.push((index + 2, line.trim_end_matches(['\r', '\n'])));
    }
    if !closed {
        return Err(ExtensionDiscoveryError::UnterminatedFrontmatter {
            path: path.to_owned(),
        });
    }
    let fields = parse_frontmatter_fields(path, &frontmatter_lines)?;
    Ok(FrontmatterDocument {
        fields,
        body: &normalized[offset..],
    })
}

pub(super) fn parse_frontmatter_fields(
    path: &Path,
    lines: &[(usize, &str)],
) -> Result<BTreeMap<String, FrontmatterValue>, ExtensionDiscoveryError> {
    let mut fields = BTreeMap::new();
    let mut index = 0;
    while index < lines.len() {
        let (line_number, raw) = lines[index];
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        if raw.starts_with(char::is_whitespace) {
            return invalid_frontmatter(path, line_number, "unexpected indentation");
        }
        let Some((raw_key, raw_value)) = line.split_once(':') else {
            return invalid_frontmatter(path, line_number, "expected `key: value`");
        };
        let key = raw_key.trim();
        if !valid_frontmatter_key(key) {
            return invalid_frontmatter(path, line_number, "invalid field name");
        }
        if fields.contains_key(key) {
            return invalid_frontmatter(path, line_number, "duplicate field");
        }
        let raw_value = raw_value.trim();
        if raw_value.is_empty() {
            let mut values = Vec::new();
            index += 1;
            while index < lines.len() {
                let (item_line, item_raw) = lines[index];
                if item_raw.trim().is_empty() {
                    index += 1;
                    continue;
                }
                let item = item_raw.trim_start();
                if !item_raw.starts_with(char::is_whitespace) || !item.starts_with('-') {
                    break;
                }
                let value = item[1..].trim();
                if value.is_empty() {
                    return invalid_frontmatter(path, item_line, "empty list item");
                }
                values.push(parse_scalar(path, item_line, value)?);
                index += 1;
            }
            fields.insert(key.to_owned(), FrontmatterValue::List(values));
            continue;
        }
        let value = if raw_value.starts_with('[') {
            FrontmatterValue::List(parse_inline_list(path, line_number, raw_value)?)
        } else {
            FrontmatterValue::Scalar(parse_scalar(path, line_number, raw_value)?)
        };
        fields.insert(key.to_owned(), value);
        index += 1;
    }
    Ok(fields)
}

pub(super) fn parse_inline_list(
    path: &Path,
    line: usize,
    value: &str,
) -> Result<Vec<String>, ExtensionDiscoveryError> {
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return invalid_frontmatter(path, line, "unterminated inline list");
    };
    let mut items = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let chars: Vec<char> = inner.chars().collect();
    for (index, character) in chars.iter().copied().enumerate() {
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(active), current) if active == current => quote = None,
            (None, ',') => {
                let item: String = chars[start..index].iter().collect();
                if !item.trim().is_empty() {
                    items.push(parse_scalar(path, line, item.trim())?);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return invalid_frontmatter(path, line, "unterminated quoted scalar");
    }
    let item: String = chars[start..].iter().collect();
    if !item.trim().is_empty() {
        items.push(parse_scalar(path, line, item.trim())?);
    }
    Ok(items)
}

pub(super) fn parse_scalar(
    path: &Path,
    line: usize,
    value: &str,
) -> Result<String, ExtensionDiscoveryError> {
    if let Some(quoted) = value.strip_prefix('"') {
        if !value.ends_with('"') || value.len() < 2 {
            return invalid_frontmatter(path, line, "unterminated double-quoted scalar");
        }
        let json = format!("\"{}\"", &quoted[..quoted.len() - 1]);
        return serde_json::from_str(&json).map_err(|_| {
            ExtensionDiscoveryError::InvalidFrontmatter {
                path: path.to_owned(),
                line,
                message: "invalid double-quoted scalar".to_owned(),
            }
        });
    }
    if let Some(quoted) = value.strip_prefix('\'') {
        if !value.ends_with('\'') || value.len() < 2 {
            return invalid_frontmatter(path, line, "unterminated single-quoted scalar");
        }
        return Ok(quoted[..quoted.len() - 1].replace("''", "'"));
    }
    Ok(value.trim().to_owned())
}

pub(super) fn parse_template(
    path: &Path,
    body: &str,
) -> Result<CommandTemplate, ExtensionDiscoveryError> {
    let mut parts = Vec::new();
    let mut text_start = 0;
    let mut cursor = 0;
    while cursor < body.len() {
        let remainder = &body[cursor..];
        let parsed = if remainder.starts_with("$ARGUMENTS") {
            Some(("$ARGUMENTS".len(), TemplatePart::Arguments))
        } else if let Some(after_dollar) = remainder.strip_prefix('$') {
            let digits = after_dollar.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                let position = after_dollar[..digits].parse::<usize>().ok();
                position
                    .filter(|position| *position > 0)
                    .map(|position| (digits + 1, TemplatePart::PositionalArgument(position)))
            } else {
                None
            }
        } else if let Some(command) = remainder.strip_prefix("!`") {
            let Some(end) = command.find('`') else {
                return Err(ExtensionDiscoveryError::UnterminatedShellInterpolation {
                    path: path.to_owned(),
                });
            };
            Some((
                end + 3,
                TemplatePart::ShellInterpolation {
                    command: command[..end].to_owned(),
                },
            ))
        } else if remainder.starts_with('@') && is_token_boundary(body, cursor) {
            let candidate_length = remainder[1..]
                .char_indices()
                .take_while(|(_, character)| is_file_reference_character(*character))
                .last()
                .map_or(0, |(index, character)| index + character.len_utf8());
            let candidate = remainder.get(1..=candidate_length).unwrap_or_default();
            let path_value = candidate.trim_end_matches('.');
            let length = path_value.len();
            (length > 0).then(|| {
                (
                    length + 1,
                    TemplatePart::FileInclusion {
                        path: path_value.to_owned(),
                    },
                )
            })
        } else {
            None
        };
        if let Some((consumed, part)) = parsed {
            push_text(&mut parts, &body[text_start..cursor]);
            parts.push(part);
            cursor += consumed;
            text_start = cursor;
        } else {
            let Some(character) = remainder.chars().next() else {
                break;
            };
            cursor += character.len_utf8();
        }
    }
    push_text(&mut parts, &body[text_start..]);
    Ok(CommandTemplate { parts })
}

pub(super) fn push_text(parts: &mut Vec<TemplatePart>, text: &str) {
    if !text.is_empty() {
        parts.push(TemplatePart::Text(text.to_owned()));
    }
}

pub(super) fn is_token_boundary(body: &str, cursor: usize) -> bool {
    cursor == 0
        || body[..cursor]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_whitespace() || "([{<\"'=:".contains(character))
}

pub(super) fn is_file_reference_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '.' | '_' | '-' | '/' | '\\')
}

pub(super) fn required_scalar(
    path: &Path,
    fields: &BTreeMap<String, FrontmatterValue>,
    field: &'static str,
) -> Result<String, ExtensionDiscoveryError> {
    optional_scalar(path, fields, field)?.ok_or_else(|| ExtensionDiscoveryError::MissingField {
        path: path.to_owned(),
        field,
    })
}

pub(super) fn optional_scalar(
    path: &Path,
    fields: &BTreeMap<String, FrontmatterValue>,
    field: &'static str,
) -> Result<Option<String>, ExtensionDiscoveryError> {
    match fields.get(field) {
        None => Ok(None),
        Some(FrontmatterValue::Scalar(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(ExtensionDiscoveryError::InvalidFrontmatter {
            path: path.to_owned(),
            line: 1,
            message: format!("`{field}` must be a non-empty scalar"),
        }),
    }
}

pub(super) fn optional_list(
    fields: &BTreeMap<String, FrontmatterValue>,
    field: &'static str,
) -> Vec<String> {
    match fields.get(field) {
        None => Vec::new(),
        Some(FrontmatterValue::List(values)) => deduplicate(values.clone()),
        Some(FrontmatterValue::Scalar(value)) => deduplicate(
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
    }
}
