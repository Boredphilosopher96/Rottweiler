//! `allowed-tools` normalization for declarative commands and skills.
//!
//! Following Claude Code, listed tools are pre-approved: matching invocations
//! run without an approval prompt while that command or skill's turn runs.
//! The list never narrows which tools are available. Entries are matched
//! against the live tool registry after mapping the Claude Code tool
//! vocabulary onto Rottweiler's canonical names. Unknown or malformed entries
//! are ignored with a per-artifact note; they never fail session startup.

use globset::GlobBuilder;
use rw_tools::{SKILL_TOOL_NAME, ToolRegistry};

/// Result of normalizing one artifact's `allowed-tools` declaration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct NormalizedAllowedTools {
    /// Turn-scoped `tool(glob)` pre-approvals for the permission gate.
    pub(super) pre_approvals: Vec<String>,
    /// Human-readable notes for ignored entries.
    pub(super) ignored: Vec<String>,
}

/// Claude Code tool names whose Rottweiler name differs by more than case.
fn canonical_tool_name(configured: &str) -> String {
    let mapped = match configured {
        "AskUserQuestion" => Some("ask_user"),
        "Agent" | "Task" => Some("spawn_agent"),
        "TodoWrite" | "TodoRead" => Some("todo"),
        "MultiEdit" => Some("multi_edit"),
        "BashOutput" => Some("background_output"),
        "KillBash" | "KillShell" => Some("background_kill"),
        "WebFetch" => Some("webfetch"),
        "WebSearch" => Some("websearch"),
        "Skill" => Some(SKILL_TOOL_NAME),
        _ => None,
    };
    mapped.map_or_else(
        || {
            configured
                .chars()
                .map(|character| match character {
                    '-' => '_',
                    character => character.to_ascii_lowercase(),
                })
                .collect()
        },
        str::to_owned,
    )
}

/// Claude Code writes command prefixes as `Bash(git status:*)`.
fn canonical_pattern(pattern: &str) -> String {
    pattern
        .strip_suffix(":*")
        .map_or_else(|| pattern.to_owned(), |prefix| format!("{prefix}*"))
}

fn valid_permission_glob(pattern: &str) -> bool {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .is_ok()
}

pub(super) fn normalize_allowed_tools(
    configured: &[String],
    tools: &ToolRegistry,
) -> NormalizedAllowedTools {
    let mut normalized = NormalizedAllowedTools::default();
    for entry in configured {
        let entry = entry.trim();
        let (base, pattern) = match entry.split_once('(') {
            Some((base, rest)) => {
                let Some(pattern) = rest.strip_suffix(')') else {
                    normalized
                        .ignored
                        .push(format!("ignored allowed tool `{entry}`: missing `)`"));
                    continue;
                };
                (base.trim(), canonical_pattern(pattern))
            }
            None => (entry, "*".to_owned()),
        };
        let name = canonical_tool_name(base);
        if name.is_empty() || tools.descriptor(&name).is_none() {
            normalized.ignored.push(format!(
                "ignored allowed tool `{entry}`: no Rottweiler tool of that name"
            ));
            continue;
        }
        if !valid_permission_glob(&pattern) {
            normalized.ignored.push(format!(
                "ignored allowed tool `{entry}`: invalid argument pattern"
            ));
            continue;
        }
        let rule = format!("{name}({pattern})");
        if !normalized.pre_approvals.contains(&rule) {
            normalized.pre_approvals.push(rule);
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use rw_tools::{ReadTool, ToolLimits};
    use std::sync::Arc;

    fn registry() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(ReadTool::new(ToolLimits::default())))
            .expect("read");
        tools
    }

    #[test]
    fn claude_names_map_to_pre_approvals_and_unknown_entries_are_ignored() {
        let normalized = normalize_allowed_tools(
            &[
                "Read".to_owned(),
                "Read(src/**)".to_owned(),
                "Read".to_owned(),
                "AskUserQuestion".to_owned(),
                "Bash(git status:*)".to_owned(),
                "Broken(".to_owned(),
            ],
            &registry(),
        );
        assert_eq!(normalized.pre_approvals, ["read(*)", "read(src/**)"]);
        assert_eq!(normalized.ignored.len(), 3);
        assert!(normalized.ignored[0].contains("AskUserQuestion"));
    }

    #[test]
    fn no_listed_tool_means_no_pre_approval() {
        assert_eq!(
            normalize_allowed_tools(&["NotebookEdit".to_owned()], &registry()).pre_approvals,
            Vec::<String>::new()
        );
        assert_eq!(
            normalize_allowed_tools(&[], &registry()),
            NormalizedAllowedTools::default()
        );
    }

    #[test]
    fn claude_prefix_patterns_become_globs() {
        assert_eq!(canonical_pattern("git status:*"), "git status*");
        assert_eq!(canonical_tool_name("WebFetch"), "webfetch");
        assert_eq!(canonical_tool_name("Agent"), "spawn_agent");
        assert_eq!(canonical_tool_name("Skill"), SKILL_TOOL_NAME);
        assert_eq!(canonical_tool_name("multi-edit"), "multi_edit");
    }
}
