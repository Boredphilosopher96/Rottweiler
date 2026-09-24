//! YAML frontmatter subset shared by markdown commands, skills, and agents.
//!
//! The accepted subset covers the forms used by SKILL.md and slash-command
//! ecosystems: plain, quoted, and multi-line plain scalars; literal and folded
//! block scalars with chomping and indentation indicators; block and inline
//! sequences. Nested mappings or sequences of mappings under any key are kept
//! as an opaque [`FrontmatterValue::Nested`] value so keys Rottweiler does not
//! interpret (for example Claude Code `hooks:`) never reject the artifact.

use super::{BTreeMap, ExtensionDiscoveryError, Path, invalid_frontmatter};

#[derive(Debug)]
pub(super) struct FrontmatterDocument<'a> {
    pub(super) fields: BTreeMap<String, FrontmatterValue>,
    pub(super) body: &'a str,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum FrontmatterValue {
    Scalar(String),
    List(Vec<String>),
    /// A mapping, a sequence of mappings, or another nested structure.
    Nested,
}

/// Splits `---` frontmatter from the markdown body. A document without an
/// opening delimiter is rejected here; callers that permit bare markdown check
/// [`has_frontmatter`] first.
pub(super) fn parse_frontmatter<'a>(
    path: &Path,
    contents: &'a str,
) -> Result<FrontmatterDocument<'a>, ExtensionDiscoveryError> {
    let normalized = strip_bom(contents);
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

pub(super) fn has_frontmatter(contents: &str) -> bool {
    strip_bom(contents)
        .split_inclusive('\n')
        .next()
        .is_some_and(|first| first.trim_end_matches(['\r', '\n']) == "---")
}

fn strip_bom(contents: &str) -> &str {
    contents.strip_prefix('\u{feff}').unwrap_or(contents)
}

type Line<'a> = (usize, &'a str);

pub(super) fn parse_frontmatter_fields(
    path: &Path,
    lines: &[Line<'_>],
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
        let Some((raw_key, raw_value)) = split_mapping_entry(line) else {
            return invalid_frontmatter(path, line_number, "expected `key: value`");
        };
        let key = raw_key.trim();
        if !valid_frontmatter_key(key) {
            return invalid_frontmatter(path, line_number, "invalid field name");
        }
        if fields.contains_key(key) {
            return invalid_frontmatter(path, line_number, "duplicate field");
        }
        index += 1;
        let child_end = child_block_end(lines, index);
        let children = &lines[index..child_end];
        index = child_end;
        let value = parse_value(path, line_number, raw_value.trim(), children)?;
        fields.insert(key.to_owned(), value);
    }
    Ok(fields)
}

/// Keys are portable identifiers; unknown keys are accepted and ignored by
/// the artifact parsers.
fn valid_frontmatter_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Splits `key: value` at the first `:` that is followed by whitespace or the
/// end of the line.
fn split_mapping_entry(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    bytes.iter().enumerate().find_map(|(index, byte)| {
        (*byte == b':' && bytes.get(index + 1).is_none_or(u8::is_ascii_whitespace))
            .then(|| (&line[..index], &line[index + 1..]))
    })
}

/// A top-level key owns every following line that is blank, a comment,
/// indented, or a zero-indented sequence item (`- item`).
fn child_block_end(lines: &[Line<'_>], start: usize) -> usize {
    let mut end = start;
    while let Some((_, raw)) = lines.get(end) {
        if raw.trim().is_empty() || raw.starts_with(char::is_whitespace) || is_sequence_item(raw) {
            end += 1;
        } else {
            break;
        }
    }
    end
}

fn is_sequence_item(raw: &str) -> bool {
    raw == "-" || raw.starts_with("- ")
}

fn indentation(raw: &str) -> usize {
    raw.len() - raw.trim_start_matches([' ', '\t']).len()
}

fn parse_value(
    path: &Path,
    line: usize,
    value: &str,
    children: &[Line<'_>],
) -> Result<FrontmatterValue, ExtensionDiscoveryError> {
    if let Some(header) = block_scalar_header(value) {
        return parse_block_scalar(path, header, children).map(FrontmatterValue::Scalar);
    }
    let content = children
        .iter()
        .filter(|(_, raw)| !raw.trim().is_empty() && !raw.trim_start().starts_with('#'))
        .copied()
        .collect::<Vec<_>>();
    if value.is_empty() {
        return parse_block_collection(path, &content);
    }
    if value.starts_with('[') {
        if !content.is_empty() {
            return Ok(FrontmatterValue::Nested);
        }
        return parse_inline_list(path, line, value).map(FrontmatterValue::List);
    }
    if value.starts_with('{') {
        return Ok(FrontmatterValue::Nested);
    }
    if value.starts_with(['"', '\'']) {
        if !content.is_empty() {
            return invalid_frontmatter(path, line, "multi-line quoted scalars are not supported");
        }
        return parse_scalar(path, line, value).map(FrontmatterValue::Scalar);
    }
    let mut scalar = strip_plain_comment(value).to_owned();
    // Multi-line plain scalar: continuation lines fold with single spaces;
    // blank lines between them become line breaks.
    let mut pending_breaks = 0_usize;
    for (_, raw) in children {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            pending_breaks += 1;
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if pending_breaks == 0 {
            scalar.push(' ');
        } else {
            scalar.extend(std::iter::repeat_n('\n', pending_breaks));
        }
        pending_breaks = 0;
        scalar.push_str(strip_plain_comment(trimmed));
    }
    Ok(FrontmatterValue::Scalar(scalar))
}

/// Block sequence of scalars, or an opaque nested structure.
fn parse_block_collection(
    path: &Path,
    content: &[Line<'_>],
) -> Result<FrontmatterValue, ExtensionDiscoveryError> {
    let Some(&(_, first)) = content.first() else {
        return Ok(FrontmatterValue::List(Vec::new()));
    };
    let item_indent = indentation(first);
    let mut values = Vec::new();
    for (position, &(line, raw)) in content.iter().enumerate() {
        let body = &raw[indentation(raw)..];
        if indentation(raw) != item_indent || !is_sequence_item(body) {
            return Ok(FrontmatterValue::Nested);
        }
        let item = body[1..].trim();
        if item.is_empty() {
            let has_nested_child = content
                .get(position + 1)
                .is_some_and(|(_, next)| indentation(next) > item_indent);
            if has_nested_child {
                return Ok(FrontmatterValue::Nested);
            }
            return invalid_frontmatter(path, line, "empty list item");
        }
        if item.starts_with(['[', '{', '|', '>'])
            || (!item.starts_with(['"', '\'']) && split_mapping_entry(item).is_some())
        {
            return Ok(FrontmatterValue::Nested);
        }
        values.push(if item.starts_with(['"', '\'']) {
            parse_scalar(path, line, item)?
        } else {
            strip_plain_comment(item).to_owned()
        });
    }
    Ok(FrontmatterValue::List(values))
}

#[derive(Clone, Copy, Debug)]
struct BlockScalarHeader {
    folded: bool,
    chomping: Chomping,
    indent: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Chomping {
    Clip,
    Strip,
    Keep,
}

fn block_scalar_header(value: &str) -> Option<BlockScalarHeader> {
    let value = strip_plain_comment(value);
    let mut characters = value.chars();
    let folded = match characters.next()? {
        '|' => false,
        '>' => true,
        _ => return None,
    };
    let mut chomping = Chomping::Clip;
    let mut indent = None;
    for character in characters {
        match character {
            '-' if chomping == Chomping::Clip => chomping = Chomping::Strip,
            '+' if chomping == Chomping::Clip => chomping = Chomping::Keep,
            '1'..='9' if indent.is_none() => {
                indent = character.to_digit(10).map(|digit| digit as usize);
            }
            _ => return None,
        }
    }
    Some(BlockScalarHeader {
        folded,
        chomping,
        indent,
    })
}

fn parse_block_scalar(
    path: &Path,
    header: BlockScalarHeader,
    children: &[Line<'_>],
) -> Result<String, ExtensionDiscoveryError> {
    let indent = match header.indent {
        Some(indent) => indent,
        None => children
            .iter()
            .find(|(_, raw)| !raw.trim().is_empty())
            .map_or(0, |(_, raw)| indentation(raw)),
    };
    let mut lines = Vec::with_capacity(children.len());
    for &(line, raw) in children {
        if raw.trim().is_empty() {
            lines.push("");
            continue;
        }
        if indent == 0 || indentation(raw) < indent {
            return invalid_frontmatter(path, line, "block scalar line is under-indented");
        }
        lines.push(&raw[indent..]);
    }
    let trailing_blank = lines
        .iter()
        .rev()
        .take_while(|line| line.is_empty())
        .count();
    let content = &lines[..lines.len() - trailing_blank];
    let mut text = if header.folded {
        fold_lines(content)
    } else {
        content.join("\n")
    };
    if !content.is_empty() {
        match header.chomping {
            Chomping::Strip => {}
            Chomping::Clip => text.push('\n'),
            Chomping::Keep => {
                text.push('\n');
                text.extend(std::iter::repeat_n('\n', trailing_blank));
            }
        }
    }
    Ok(text)
}

/// YAML folding: adjacent normal lines join with a space, each blank line is
/// one line break, and more-indented lines keep their line breaks.
fn fold_lines(lines: &[&str]) -> String {
    let mut text = String::new();
    let mut pending_breaks = 0_usize;
    let mut previous_more_indented: Option<bool> = None;
    for line in lines {
        if line.is_empty() {
            pending_breaks += 1;
            continue;
        }
        let more_indented = line.starts_with([' ', '\t']);
        match previous_more_indented {
            None => text.extend(std::iter::repeat_n('\n', pending_breaks)),
            Some(previous) if !previous && !more_indented => {
                if pending_breaks == 0 {
                    text.push(' ');
                } else {
                    text.extend(std::iter::repeat_n('\n', pending_breaks));
                }
            }
            Some(_) => text.extend(std::iter::repeat_n('\n', pending_breaks + 1)),
        }
        text.push_str(line);
        pending_breaks = 0;
        previous_more_indented = Some(more_indented);
    }
    text
}

/// Removes a ` #` comment from a plain scalar.
fn strip_plain_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let cut = bytes
        .iter()
        .enumerate()
        .find_map(|(index, byte)| {
            (*byte == b'#' && index > 0 && bytes[index - 1].is_ascii_whitespace()).then_some(index)
        })
        .unwrap_or(value.len());
    value[..cut].trim_end()
}

pub(super) fn parse_inline_list(
    path: &Path,
    line: usize,
    value: &str,
) -> Result<Vec<String>, ExtensionDiscoveryError> {
    let value = strip_plain_comment(value);
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
        let Some(end) = closing_double_quote(quoted) else {
            return invalid_frontmatter(path, line, "unterminated double-quoted scalar");
        };
        if !trailing_is_comment(&quoted[end + 1..]) {
            return invalid_frontmatter(path, line, "unexpected text after quoted scalar");
        }
        let json = format!("\"{}\"", &quoted[..end]);
        return serde_json::from_str(&json).map_err(|_| {
            ExtensionDiscoveryError::InvalidFrontmatter {
                path: path.to_owned(),
                line,
                message: "invalid double-quoted scalar".to_owned(),
            }
        });
    }
    if let Some(quoted) = value.strip_prefix('\'') {
        let Some(end) = closing_single_quote(quoted) else {
            return invalid_frontmatter(path, line, "unterminated single-quoted scalar");
        };
        if !trailing_is_comment(&quoted[end + 1..]) {
            return invalid_frontmatter(path, line, "unexpected text after quoted scalar");
        }
        return Ok(quoted[..end].replace("''", "'"));
    }
    Ok(strip_plain_comment(value).trim().to_owned())
}

fn closing_double_quote(quoted: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, byte) in quoted.bytes().enumerate() {
        match byte {
            b'\\' if !escaped => escaped = true,
            b'"' if !escaped => return Some(index),
            _ => escaped = false,
        }
    }
    None
}

fn closing_single_quote(quoted: &str) -> Option<usize> {
    let bytes = quoted.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            if bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
                continue;
            }
            return Some(index);
        }
        index += 1;
    }
    None
}

fn trailing_is_comment(rest: &str) -> bool {
    let rest = rest.trim_start();
    rest.is_empty() || rest.starts_with('#')
}

#[cfg(test)]
mod tests;
