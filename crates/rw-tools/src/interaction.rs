mod presentation;
use presentation::{ANSWER_PRESENTATION, PLAN_PRESENTATION};

use std::sync::Arc;

use async_trait::async_trait;
use rw_types::PlanArtifact;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::registry::{
    CancellationToken, CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError,
    ToolLimits, ToolResult, input_schema, parse_input,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskUserInput {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default = "default_true")]
    pub allow_free_text: bool,
}

const fn default_true() -> bool {
    true
}

/// Read-only control tool used by Plan mode to hand a structured artifact to
/// the engine. Core owns approval and durable state; this tool only validates
/// and returns the provider-neutral payload.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubmitPlanTool;

#[async_trait]
impl Tool for SubmitPlanTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "submit_plan".to_owned(),
            description: "Submit the complete implementation plan for explicit user approval."
                .to_owned(),
            input_schema: input_schema::<PlanArtifact>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    fn behavior(&self) -> crate::ToolBehavior {
        crate::ToolBehavior::PlanSubmission
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let artifact: PlanArtifact = parse_input(input)?;
        if artifact.title.trim().is_empty()
            || artifact.summary_md.trim().is_empty()
            || artifact.steps.is_empty()
            || artifact.steps.iter().any(|step| {
                step.description.trim().is_empty() || step.verification.trim().is_empty()
            })
        {
            return Err(ToolError::InvalidInput(
                "plan requires a title, summary, and at least one described/verifiable step"
                    .to_owned(),
            ));
        }
        let data = serde_json::to_value(&artifact)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        Ok(ToolResult::new("plan submitted for approval", data)
            .with_presentation(PLAN_PRESENTATION.plan()?))
    }
}

/// Injected UI boundary. Headless callers can supply their own transport.
#[async_trait]
pub trait QuestionAsker: Send + Sync {
    async fn ask(
        &self,
        request: AskUserInput,
        cancellation: CancellationToken,
    ) -> Result<String, ToolError>;
}

#[derive(Clone)]
pub struct AskUserTool {
    asker: Arc<dyn QuestionAsker>,
    max_answer_bytes: usize,
}

impl AskUserTool {
    #[must_use]
    pub fn new(asker: Arc<dyn QuestionAsker>, limits: ToolLimits) -> Self {
        Self {
            asker,
            max_answer_bytes: limits.max_result_bytes,
        }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ask_user".to_owned(),
            description: "Ask the user one focused question through the host UI boundary."
                .to_owned(),
            input_schema: input_schema::<AskUserInput>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    fn behavior(&self) -> crate::ToolBehavior {
        crate::ToolBehavior::UserInteraction
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: AskUserInput = parse_input(input)?;
        if input.question.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "question must not be empty".to_owned(),
            ));
        }
        if input.options.iter().any(|option| option.trim().is_empty()) {
            return Err(ToolError::InvalidInput(
                "options must not contain empty values".to_owned(),
            ));
        }
        let answer = tokio::select! {
            answer = context.question_asker().unwrap_or(&self.asker).ask(
                input,
                context.cancellation.clone(),
            ) => answer?,
            () = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
        };
        if answer.len() > self.max_answer_bytes {
            return Err(ToolError::SizeLimit {
                limit: self.max_answer_bytes,
            });
        }
        Ok(ToolResult::new(answer.clone(), json!({"answer": answer}))
            .with_presentation(ANSWER_PRESENTATION.plan()?))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use tempfile::tempdir;

    use super::*;

    struct MockAsker;

    #[async_trait]
    impl QuestionAsker for MockAsker {
        async fn ask(
            &self,
            request: AskUserInput,
            _cancellation: CancellationToken,
        ) -> Result<String, ToolError> {
            assert_eq!(request.question, "Continue?");
            Ok("yes".to_owned())
        }
    }

    #[tokio::test]
    async fn ask_user_uses_the_injected_boundary() {
        let root = tempdir().expect("temp directory");
        let context = ToolContext::new(root.path()).expect("context");
        let result = AskUserTool::new(Arc::new(MockAsker), ToolLimits::default())
            .execute(
                &context,
                serde_json::json!({"question": "Continue?", "options": ["yes", "no"]}),
            )
            .await
            .expect("answer");
        assert_eq!(result.data["answer"], "yes");
    }
}
