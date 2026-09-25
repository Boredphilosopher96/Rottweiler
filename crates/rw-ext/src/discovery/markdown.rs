use super::filesystem::SkillManifest;
use super::frontmatter::{FrontmatterValue, has_frontmatter, parse_frontmatter};
use super::{
    ArtifactLocation, ArtifactOrigin, ArtifactScope, BTreeMap, CommandTemplate, DiscoveredAgent,
    DiscoveredCommand, DiscoveredSkill, ExtensionDiscoveryError, LazyMarkdownBody,
    MAX_MARKDOWN_BYTES, Path, TemplatePart, deduplicate, file_stem, read_bounded_relative_utf8,
    read_bounded_utf8, validate_artifact_name, validate_mcp_virtual_tool,
};

/// Longest description derived from a command body.
const MAX_DERIVED_DESCRIPTION_CHARS: usize = 120;

/// Commands are markdown prompts. Frontmatter is optional; without a
/// `description` the first non-empty body line describes the command.
pub(super) fn discover_command(
    scope: ArtifactScope,
    location: ArtifactLocation,
    root: &Path,
    path: &Path,
) -> Result<DiscoveredCommand, ExtensionDiscoveryError> {
    let contents = read_bounded_utf8(path, MAX_MARKDOWN_BYTES)?;
    let digest = blake3::hash(contents.as_bytes());
    let (fields, body) = if has_frontmatter(&contents) {
        let document = parse_frontmatter(path, &contents)?;
        (document.fields, document.body)
    } else {
        (BTreeMap::new(), contents.as_str())
    };
    let name = file_stem(path)?;
    validate_artifact_name(path, &name)?;
    let description = match optional_scalar(path, &fields, "description")? {
        Some(description) => description,
        None => derived_description(body).ok_or_else(|| ExtensionDiscoveryError::MissingField {
            path: path.to_owned(),
            field: "description",
        })?,
    };
    let model = optional_scalar(path, &fields, "model")?;
    let allowed_tools = optional_list(&fields, "allowed-tools");
    let argument_hint = optional_scalar(path, &fields, "argument-hint")?;
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

fn derived_description(body: &str) -> Option<String> {
    let line = body
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())?;
    let mut description = line
        .chars()
        .take(MAX_DERIVED_DESCRIPTION_CHARS)
        .collect::<String>();
    if line.chars().count() > MAX_DERIVED_DESCRIPTION_CHARS {
        description.push('…');
    }
    Some(description)
}

/// A skill is identified by its directory name, as in the Agent Skills and
/// Claude Code conventions; a differing frontmatter `name` does not rename
/// it. SKILL.md requires `description`; keys Rottweiler does not interpret
/// are ignored.
pub(super) fn discover_skill(
    scope: ArtifactScope,
    location: ArtifactLocation,
    manifest: SkillManifest,
) -> Result<DiscoveredSkill, ExtensionDiscoveryError> {
    let path = manifest.path;
    let bytes = super::read_bounded_relative_file(
        &manifest.root,
        Path::new("SKILL.md"),
        MAX_MARKDOWN_BYTES,
    )?;
    let contents = String::from_utf8(bytes)
        .map_err(|_| ExtensionDiscoveryError::NotUtf8 { path: path.clone() })?;
    let document = parse_frontmatter(&path, &contents)?;
    let name = manifest.entry_name;
    validate_artifact_name(&path, &name)?;
    let description = required_scalar(&path, &document.fields, "description")?;
    let allowed_tools = optional_list(&document.fields, "allowed-tools");
    Ok(DiscoveredSkill {
        name,
        description,
        allowed_tools,
        origin: ArtifactOrigin {
            scope,
            location,
            path,
        },
        root: manifest.root,
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

/// Positional argument numbering of a command source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ArgumentIndexing {
    /// `$1` is the first argument.
    OneBased,
    /// Claude Code numbering: `$0` and `$ARGUMENTS[0]` are the first argument.
    ZeroBased,
}

pub(super) fn parse_template(
    path: &Path,
    body: &str,
    indexing: ArgumentIndexing,
) -> Result<CommandTemplate, ExtensionDiscoveryError> {
    let mut parts = Vec::new();
    let mut text_start = 0;
    let mut cursor = 0;
    let first_position = match indexing {
        ArgumentIndexing::OneBased => 1,
        ArgumentIndexing::ZeroBased => 0,
    };
    let positional = |digits: &str| {
        digits
            .parse::<usize>()
            .ok()
            .filter(|position| *position >= first_position)
            .map(|position| TemplatePart::PositionalArgument(position + 1 - first_position))
    };
    while cursor < body.len() {
        let remainder = &body[cursor..];
        let indexed_arguments = remainder
            .strip_prefix("$ARGUMENTS[")
            .filter(|_| indexing == ArgumentIndexing::ZeroBased)
            .and_then(|rest| {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                (digits > 0 && rest[digits..].starts_with(']'))
                    .then(|| {
                        positional(&rest[..digits])
                            .map(|part| ("$ARGUMENTS[]".len() + digits, part))
                    })
                    .flatten()
            });
        let parsed = if indexed_arguments.is_some() {
            indexed_arguments
        } else if remainder.starts_with("$ARGUMENTS") {
            Some(("$ARGUMENTS".len(), TemplatePart::Arguments))
        } else if let Some(after_dollar) = remainder.strip_prefix('$') {
            let digits = after_dollar.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                positional(&after_dollar[..digits]).map(|part| (digits + 1, part))
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
        Some(FrontmatterValue::Scalar(value)) if !value.trim().is_empty() => {
            Ok(Some(value.trim().to_owned()))
        }
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
        None | Some(FrontmatterValue::Nested) => Vec::new(),
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
