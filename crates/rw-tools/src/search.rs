use std::path::PathBuf;

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, sinks::UTF8};
use ignore::WalkBuilder;
use rw_types::ToolCapability;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::registry::{
    CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError, ToolLimits, ToolResult,
    input_schema, parse_input,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepInput {
    pub pattern: String,
    #[serde(default = "default_path")]
    pub path: PathBuf,
    pub glob: Option<String>,
    #[serde(default)]
    pub case_insensitive: bool,
}

#[derive(Clone, Debug)]
pub struct GrepTool {
    limits: ToolLimits,
}

impl GrepTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self { limits }
    }
}

#[derive(Debug, Serialize)]
struct GrepMatch {
    path: PathBuf,
    line: u64,
    text: String,
}

#[async_trait]
impl Tool for GrepTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        descriptor::<GrepInput>(
            "grep",
            "Search workspace text with ripgrep's regex and ignore engines.",
        )
    }

    fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(vec![parse_input::<GrepInput>(input.clone())?.path])
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: GrepInput = parse_input(input)?;
        if input.pattern.is_empty() {
            return Err(ToolError::InvalidInput(
                "pattern must not be empty".to_owned(),
            ));
        }
        let roots = context.resolve_search_roots(&input.path)?;
        let regex = RegexMatcherBuilder::new()
            .case_insensitive(input.case_insensitive)
            .build(&input.pattern)
            .map_err(|error| ToolError::InvalidInput(format!("invalid regex: {error}")))?;
        let glob = input.glob.as_deref().map(compile_glob).transpose()?;
        let mut findings = Vec::new();
        let mut result_bytes = 0usize;
        let mut truncated = false;

        for root in roots {
            for entry in WalkBuilder::new(&root)
                .standard_filters(true)
                .follow_links(false)
                .sort_by_file_path(std::path::Path::cmp)
                .build()
                .filter_map(Result::ok)
            {
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
                let mut searcher = SearcherBuilder::new().line_number(true).build();
                searcher
                    .search_path(
                        &regex,
                        entry.path(),
                        UTF8(|line, text| {
                            if context.cancellation.is_cancelled()
                                || findings.len() >= self.limits.max_search_results
                            {
                                truncated = true;
                                return Ok(false);
                            }
                            let text = text.trim_end_matches(['\n', '\r']).to_owned();
                            let prospective = relative.as_os_str().len() + text.len() + 32;
                            if result_bytes.saturating_add(prospective)
                                > self.limits.max_result_bytes
                            {
                                truncated = true;
                                return Ok(false);
                            }
                            result_bytes = result_bytes.saturating_add(prospective);
                            findings.push(GrepMatch {
                                path: relative.clone(),
                                line,
                                text,
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
        );
        result.truncated = truncated;
        Ok(result)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobInput {
    pub pattern: String,
    #[serde(default = "default_path")]
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct GlobTool {
    limits: ToolLimits,
}

impl GlobTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self { limits }
    }
}

#[async_trait]
impl Tool for GlobTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        descriptor::<GlobInput>("glob", "List non-ignored workspace paths matching a glob.")
    }

    fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(vec![parse_input::<GlobInput>(input.clone())?.path])
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: GlobInput = parse_input(input)?;
        let roots = context.resolve_search_roots(&input.path)?;
        let matcher = compile_glob(&input.pattern)?;
        let mut paths = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        for root in roots {
            for entry in WalkBuilder::new(&root)
                .standard_filters(true)
                .follow_links(false)
                .sort_by_file_path(std::path::Path::cmp)
                .build()
                .filter_map(Result::ok)
            {
                context.cancellation.check()?;
                if entry.path() == root {
                    continue;
                }
                let relative = context.relative_display(entry.path());
                if !matcher.is_match(&relative) {
                    continue;
                }
                let length = relative.as_os_str().len().saturating_add(1);
                if paths.len() >= self.limits.max_search_results
                    || bytes.saturating_add(length) > self.limits.max_result_bytes
                {
                    truncated = true;
                    break;
                }
                bytes = bytes.saturating_add(length);
                paths.push(relative);
            }
            if truncated {
                break;
            }
        }
        paths.sort();
        let model_text = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut result = ToolResult::new(
            model_text,
            json!({"paths": paths, "count": paths.len(), "truncated": truncated}),
        );
        result.truncated = truncated;
        Ok(result)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LsInput {
    #[serde(default = "default_path")]
    pub path: PathBuf,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Clone, Debug)]
pub struct LsTool {
    limits: ToolLimits,
}

impl LsTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self { limits }
    }
}

#[derive(Debug, Serialize)]
struct LsEntry {
    path: PathBuf,
    kind: &'static str,
    bytes: Option<u64>,
}

#[async_trait]
impl Tool for LsTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        descriptor::<LsInput>("ls", "List workspace directory entries and basic metadata.")
    }

    fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(vec![parse_input::<LsInput>(input.clone())?.path])
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: LsInput = parse_input(input)?;
        let roots = context.resolve_search_roots(&input.path)?;
        let mut entries = Vec::new();
        let mut result_bytes = 0usize;
        let mut truncated = false;
        for root in roots {
            if !root.is_dir() {
                return Err(ToolError::InvalidInput(format!(
                    "{} is not a directory",
                    input.path.display()
                )));
            }
            let iterator = WalkBuilder::new(&root)
                .max_depth(if input.recursive { None } else { Some(1) })
                .standard_filters(true)
                .follow_links(false)
                .sort_by_file_path(std::path::Path::cmp)
                .build();
            for entry in iterator.filter_map(Result::ok) {
                context.cancellation.check()?;
                if entry.path() == root {
                    continue;
                }
                if entries.len() >= self.limits.max_directory_entries {
                    truncated = true;
                    break;
                }
                let metadata = entry.metadata().ok();
                let kind = metadata.as_ref().map_or("other", |metadata| {
                    if metadata.is_dir() {
                        "directory"
                    } else if metadata.is_file() {
                        "file"
                    } else if metadata.file_type().is_symlink() {
                        "symlink"
                    } else {
                        "other"
                    }
                });
                let path = context.relative_display(entry.path());
                let prospective = path.as_os_str().len().saturating_add(12);
                if result_bytes.saturating_add(prospective) > self.limits.max_result_bytes {
                    truncated = true;
                    break;
                }
                result_bytes = result_bytes.saturating_add(prospective);
                entries.push(LsEntry {
                    path,
                    kind,
                    bytes: metadata
                        .filter(std::fs::Metadata::is_file)
                        .map(|value| value.len()),
                });
            }
            if truncated {
                break;
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let model_text = entries
            .iter()
            .map(|entry| format!("{:<9} {}", entry.kind, entry.path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        let mut result = ToolResult::new(
            model_text,
            json!({"entries": entries, "count": entries.len(), "truncated": truncated}),
        );
        result.truncated = truncated;
        Ok(result)
    }
}

fn default_path() -> PathBuf {
    PathBuf::from(".")
}

fn descriptor<T: JsonSchema>(name: &str, description: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: input_schema::<T>(),
        capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
    }
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, ToolError> {
    if pattern.is_empty() {
        return Err(ToolError::InvalidInput(
            "glob pattern must not be empty".to_owned(),
        ));
    }
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher())
        .map_err(|error| ToolError::InvalidInput(format!("invalid glob: {error}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn default_search_spans_all_workspace_roots_with_virtual_paths() {
        let root = tempdir().expect("temp directory");
        let primary = root.path().join("primary");
        let added = root.path().join("added");
        fs::create_dir_all(&primary).expect("primary");
        fs::create_dir_all(&added).expect("added");
        fs::write(primary.join("primary.txt"), "needle\n").expect("primary fixture");
        fs::write(added.join("added.txt"), "needle\n").expect("added fixture");
        fs::write(root.path().join("outside.txt"), "needle\n").expect("outside fixture");
        let context = ToolContext::from_workspace_roots([&primary, &added]).expect("context");

        let grep = GrepTool::new(ToolLimits::default())
            .execute(&context, json!({"pattern": "needle"}))
            .await
            .expect("grep");
        assert!(grep.content.contains("primary.txt"));
        assert!(grep.content.contains("@root/1/added.txt"));
        assert!(!grep.content.contains("outside.txt"));
    }

    #[tokio::test]
    async fn grep_glob_and_ls_are_deterministic_and_ignore_gitignored_files() {
        let root = tempdir().expect("temp directory");
        fs::create_dir(root.path().join("src")).expect("source directory");
        fs::write(root.path().join(".gitignore"), "ignored.txt\n").expect("ignore file");
        fs::write(root.path().join("src/b.rs"), "fn needle() {}\n").expect("b fixture");
        fs::write(root.path().join("src/a.rs"), "// needle\n").expect("a fixture");
        fs::write(root.path().join("ignored.txt"), "needle\n").expect("ignored fixture");
        let context = ToolContext::new(root.path()).expect("context");
        let limits = ToolLimits::default();

        let grep = GrepTool::new(limits)
            .execute(&context, json!({"pattern": "needle", "glob": "**/*.rs"}))
            .await
            .expect("grep");
        assert!(grep.content.contains("src/a.rs"));
        assert!(grep.content.contains("src/b.rs"));
        assert!(!grep.content.contains("ignored.txt"));

        let glob = GlobTool::new(limits)
            .execute(&context, json!({"pattern": "**/*.rs"}))
            .await
            .expect("glob");
        assert_eq!(glob.content, "src/a.rs\nsrc/b.rs");

        let ls = LsTool::new(limits)
            .execute(&context, json!({"recursive": true}))
            .await
            .expect("ls");
        let a = ls.content.find("src/a.rs").expect("a entry");
        let b = ls.content.find("src/b.rs").expect("b entry");
        assert!(a < b);
    }

    #[tokio::test]
    async fn search_respects_result_caps() {
        let root = tempdir().expect("temp directory");
        fs::write(root.path().join("z.txt"), "x\n").expect("z fixture");
        fs::write(root.path().join("a.txt"), "x\n").expect("a fixture");
        let context = ToolContext::new(root.path()).expect("context");
        let tool = GrepTool::new(ToolLimits {
            max_search_results: 1,
            ..ToolLimits::default()
        });
        let result = tool
            .execute(&context, json!({"pattern": "x"}))
            .await
            .expect("grep");
        assert!(result.truncated);
        assert_eq!(result.data["count"], 1);
        assert!(result.content.starts_with("a.txt:"), "{}", result.content);
    }
}
