//! Admitted filesystem kernel for bounded line-oriented search.
use super::{
    GrepInput, MAX_PATTERN_BYTES, MAX_REGEX_AUTOMATON_BYTES, MAX_SEARCH_LINE_BYTES, ToolContext,
    ToolError, ToolLimits, ToolResult, compile_glob, parse_input, presentation::GREP_PRESENTATION,
    walk::BoundedWalk,
};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{SearcherBuilder, sinks::UTF8};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct GrepMatch {
    path: PathBuf,
    line: u64,
    text: String,
}

pub(super) fn execute(
    context: &ToolContext,
    input: Value,
    limits: ToolLimits,
) -> Result<ToolResult, ToolError> {
    context.cancellation.check()?;
    let input: GrepInput = parse_input(input)?;
    let roots = context.resolve_search_roots(&input.path)?;
    let regex = matcher(&input)?;
    let glob = input.glob.as_deref().map(compile_glob).transpose()?;
    let mut findings = Vec::new();
    let mut result_bytes = 0usize;
    let mut truncated = false;

    for root in roots {
        for entry in BoundedWalk::new(&root, true, &context.cancellation)? {
            let entry = entry?;
            context.cancellation.check()?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = context.relative_display(entry.path());
            if glob
                .as_ref()
                .is_some_and(|matcher| !matcher.is_match(&relative))
            {
                continue;
            }
            let mut searcher = SearcherBuilder::new()
                .line_number(true)
                .heap_limit(Some(MAX_SEARCH_LINE_BYTES))
                .build();
            searcher
                .search_path(
                    &regex,
                    entry.path(),
                    UTF8(|line, text| {
                        if context.cancellation.is_cancelled()
                            || findings.len() >= limits.max_search_results
                        {
                            truncated = true;
                            return Ok(false);
                        }
                        let text = text.trim_end_matches(['\n', '\r']);
                        let prospective = relative.as_os_str().len() + text.len() + 32;
                        if result_bytes.saturating_add(prospective) > limits.max_result_bytes {
                            truncated = true;
                            return Ok(false);
                        }
                        result_bytes = result_bytes.saturating_add(prospective);
                        findings.push(GrepMatch {
                            path: relative.clone(),
                            line,
                            text: text.to_owned(),
                        });
                        Ok(true)
                    }),
                )
                .map_err(|error| ToolError::Io {
                    operation: "search file",
                    path: relative,
                    source: std::io::Error::other(error),
                })?;
            if truncated {
                break;
            }
        }
        if truncated {
            break;
        }
    }
    context.cancellation.check()?;
    let model_text = findings
        .iter()
        .map(|item| format!("{}:{}:{}", item.path.display(), item.line, item.text))
        .collect::<Vec<_>>()
        .join("\n");
    let mut result = ToolResult::new(
        model_text,
        json!({"matches": findings, "count": findings.len(), "truncated": truncated}),
    )
    .with_presentation(GREP_PRESENTATION.plan()?);
    result.truncated = truncated;
    Ok(result)
}
fn matcher(input: &GrepInput) -> Result<RegexMatcher, ToolError> {
    if input.pattern.is_empty() || input.pattern.len() > MAX_PATTERN_BYTES {
        return Err(ToolError::InvalidInput(
            "pattern must contain 1..=65536 bytes".to_owned(),
        ));
    }
    RegexMatcherBuilder::new()
        .case_insensitive(input.case_insensitive)
        .size_limit(MAX_REGEX_AUTOMATON_BYTES)
        .dfa_size_limit(MAX_REGEX_AUTOMATON_BYTES)
        .build(&input.pattern)
        .map_err(|error| ToolError::InvalidInput(format!("invalid regex: {error}")))
}
