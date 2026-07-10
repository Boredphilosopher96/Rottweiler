use std::sync::Arc;

use async_trait::async_trait;
use rw_intel::{Language, SymbolIndex, SymbolQuery, SymbolRole};
use rw_types::ToolCapability;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::registry::{
    CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError, ToolLimits, ToolResult,
    input_schema, parse_input,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolsInput {
    pub pattern: String,
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub roles: Vec<SymbolRole>,
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub languages: Vec<Language>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

const fn default_limit() -> usize {
    100
}

#[derive(Clone)]
pub struct SymbolsTool {
    index: Arc<SymbolIndex>,
    limits: ToolLimits,
}

impl SymbolsTool {
    #[must_use]
    pub fn new(index: Arc<SymbolIndex>, limits: ToolLimits) -> Self {
        Self { index, limits }
    }
}

#[async_trait]
impl Tool for SymbolsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "symbols".to_owned(),
            description: "Search incremental tree-sitter definitions and references across Rust, Python, and TypeScript."
                .to_owned(),
            input_schema: input_schema::<SymbolsInput>(),
            capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
        }
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: SymbolsInput = parse_input(input)?;
        if input.pattern.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "symbol pattern must not be empty".to_owned(),
            ));
        }
        let matches = self
            .index
            .query(&SymbolQuery {
                pattern: input.pattern,
                roles: input.roles,
                languages: input.languages,
                limit: input.limit.min(self.limits.max_search_results),
            })
            .map_err(|error| ToolError::Intelligence(error.to_string()))?;
        context.cancellation.check()?;
        let mut retained = Vec::new();
        let mut model_text = String::new();
        let mut truncated = false;
        for symbol in matches {
            let line = format!(
                "{}:{}:{} {:?} {:?} {}",
                symbol.location.path.display(),
                symbol.location.line,
                symbol.location.column,
                symbol.role,
                symbol.kind,
                symbol.name
            );
            let separator = usize::from(!model_text.is_empty());
            if model_text
                .len()
                .saturating_add(separator)
                .saturating_add(line.len())
                > self.limits.max_result_bytes
            {
                truncated = true;
                break;
            }
            if separator == 1 {
                model_text.push('\n');
            }
            model_text.push_str(&line);
            retained.push(symbol);
        }
        let mut result = ToolResult::new(
            model_text,
            json!({"matches": retained, "count": retained.len(), "truncated": truncated}),
        );
        result.truncated = truncated;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn exposes_the_incremental_index_as_a_tool() {
        let root = tempdir().expect("temp directory");
        let index = Arc::new(SymbolIndex::new(root.path()).expect("index"));
        index
            .update_source("lib.rs", "pub struct Rottweiler;")
            .expect("source");
        let context = ToolContext::new(root.path()).expect("context");
        let result = SymbolsTool::new(index, ToolLimits::default())
            .execute(
                &context,
                serde_json::json!({"pattern": "Rott", "roles": ["definition"]}),
            )
            .await
            .expect("symbols");
        assert_eq!(result.data["count"], 1);
        assert!(result.content.contains("Rottweiler"));
    }
}
