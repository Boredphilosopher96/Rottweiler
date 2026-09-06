//! One session binding to the application's shared semantic source owner.
use crate::transcript_service::TranscriptReader;
use rw_core::{AgentLoopError, ui::UiToolSource};
use rw_types::{SequenceId, SessionId, ToolInvocationId, extension_ui::UiPresentation};
use std::sync::Arc;

pub(crate) struct ToolSource {
    pub(crate) reader: Arc<TranscriptReader>,
    pub(crate) session: SessionId,
}
#[async_trait::async_trait]
impl UiToolSource for ToolSource {
    async fn presentation(
        &self,
        invocation: &ToolInvocationId,
        expected_through: Option<SequenceId>,
    ) -> Result<Option<UiPresentation>, AgentLoopError> {
        self.reader
            .tool_presentation(self.session.clone(), invocation.clone(), expected_through)
            .await
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))
    }
}
