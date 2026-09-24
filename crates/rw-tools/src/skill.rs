//! Model-invoked skill loading.
//!
//! The session's skill index names each discovered skill. When a task matches
//! a skill's description the model calls `skill` with that name and receives
//! the same instructions a user invocation delivers; bundled files are then
//! requested one at a time with `path`. The tool only reads configured skill
//! sources and never touches the workspace.

mod presentation;
use presentation::SKILL_PRESENTATION;

use std::sync::Arc;

use async_trait::async_trait;
use rw_types::ToolCapability;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::registry::{
    CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError, ToolResult, WorkspaceBinding,
    input_schema, parse_input,
};

/// Canonical registry name of the skill tool.
pub const SKILL_TOOL_NAME: &str = "skill";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillInput {
    /// Skill name exactly as listed in the available skills.
    pub name: String,
    /// Optional bundled file, relative to the skill root. Omit to load the
    /// skill's instructions.
    #[serde(default)]
    pub path: Option<String>,
}

/// Content returned by a [`SkillLibrary`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDocument {
    pub name: String,
    pub path: Option<String>,
    pub content: String,
}

/// Source of skill instructions and bundled files, supplied by the session
/// runtime from its discovered extension catalog.
pub trait SkillLibrary: Send + Sync {
    /// Loads a skill's instructions, or one bundled file when `path` is set.
    ///
    /// # Errors
    ///
    /// Returns a user-readable reason when the skill is unknown or the
    /// requested content cannot be read.
    fn load(&self, name: &str, path: Option<&str>) -> Result<SkillDocument, String>;
}

/// Read-only tool that loads a skill by name.
#[derive(Clone)]
pub struct SkillTool {
    library: Arc<dyn SkillLibrary>,
}

impl SkillTool {
    #[must_use]
    pub fn new(library: Arc<dyn SkillLibrary>) -> Self {
        Self { library }
    }
}

#[async_trait]
impl Tool for SkillTool {
    /// Reads only configured skill sources, never a workspace path, so a
    /// child agent can use its parent's instance when its own workspace
    /// discovered no skills.
    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: SKILL_TOOL_NAME.to_owned(),
            description: "Load a skill's instructions by name when the task matches its description, or one of its bundled files with `path`.".to_owned(),
            input_schema: input_schema::<SkillInput>(),
            capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
        }
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: SkillInput = parse_input(input)?;
        if input.name.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "skill name must not be empty".to_owned(),
            ));
        }
        let library = Arc::clone(&self.library);
        let document = tokio::task::spawn_blocking(move || {
            library.load(input.name.trim(), input.path.as_deref())
        })
        .await
        .map_err(|_| ToolError::InvalidInput("skill loading was interrupted".to_owned()))?
        .map_err(ToolError::InvalidInput)?;
        let data = json!({
            "name": document.name,
            "path": document.path,
            "bytes": document.content.len(),
        });
        Ok(ToolResult::new(document.content, data).with_presentation(SKILL_PRESENTATION.plan()?))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use tempfile::tempdir;

    struct Library;

    impl SkillLibrary for Library {
        fn load(&self, name: &str, path: Option<&str>) -> Result<SkillDocument, String> {
            match (name, path) {
                ("review", None) => Ok(SkillDocument {
                    name: name.to_owned(),
                    path: None,
                    content: "# Skill: review".to_owned(),
                }),
                ("review", Some("checklist.md")) => Ok(SkillDocument {
                    name: name.to_owned(),
                    path: path.map(str::to_owned),
                    content: "- item".to_owned(),
                }),
                _ => Err(format!("unknown skill `{name}`")),
            }
        }
    }

    #[tokio::test]
    async fn skill_tool_loads_instructions_and_bundled_files() {
        let root = tempdir().expect("temp directory");
        let context = ToolContext::new(root.path()).expect("context");
        let tool = SkillTool::new(Arc::new(Library));
        assert_eq!(
            tool.descriptor().capabilities.capabilities(),
            [ToolCapability::ReadFilesystem]
        );
        let result = tool
            .execute(&context, json!({"name": "review"}))
            .await
            .expect("instructions");
        assert_eq!(result.content, "# Skill: review");
        let result = tool
            .execute(&context, json!({"name": "review", "path": "checklist.md"}))
            .await
            .expect("bundled file");
        assert_eq!(result.data["path"], "checklist.md");
        let error = tool
            .execute(&context, json!({"name": "missing"}))
            .await
            .expect_err("unknown");
        assert!(error.to_string().contains("unknown skill `missing`"));
    }
}
