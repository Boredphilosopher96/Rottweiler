//! Interactive tool calls require the actor's question route.
use async_trait::async_trait;
use rw_tools::{AskUserInput, CancellationToken, QuestionAsker, ToolError};

pub(super) struct UnboundQuestionAsker;

#[async_trait]
impl QuestionAsker for UnboundQuestionAsker {
    async fn ask(
        &self,
        _request: AskUserInput,
        _cancellation: CancellationToken,
    ) -> std::result::Result<String, ToolError> {
        Err(ToolError::Interaction(
            "ask_user requires an active session question route".to_owned(),
        ))
    }
}
