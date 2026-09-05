#![cfg(test)]
use async_trait::async_trait;
use rw_core::{OrchestrationError, SubagentArtifactSource};
use rw_tools::{AuthorizedDiffArtifact, DiffArtifactAuthority, ToolError};
use rw_types::{SessionId, SubagentId, SubagentResult};
use std::{collections::HashMap, sync::Mutex};

/// Models the fixture observer's acknowledged terminal store. Production uses source selectors.
#[derive(Default)]
pub(crate) struct TestArtifactSource {
    results: Mutex<HashMap<(SessionId, SubagentId), SubagentResult>>,
}
#[async_trait]
impl DiffArtifactAuthority for TestArtifactSource {
    async fn resolve(
        &self,
        parent: &SessionId,
        id: &str,
    ) -> Result<Option<AuthorizedDiffArtifact>, ToolError> {
        let results = self
            .results
            .lock()
            .map_err(|_| ToolError::Output("fixture source poisoned".into()))?;
        Ok(results
            .iter()
            .filter(|((session, _), _)| session == parent)
            .filter_map(|(_, result)| result.diff_artifact.as_ref())
            .find(|artifact| artifact.id == id)
            .map(|artifact| AuthorizedDiffArtifact::new(artifact.clone(), ())))
    }
}
#[async_trait]
impl SubagentArtifactSource for TestArtifactSource {
    async fn verify_result(
        &self,
        parent: &SessionId,
        result: &SubagentResult,
    ) -> Result<(), OrchestrationError> {
        if let Some(artifact) = &result.diff_artifact {
            rw_tools::validate_diff_artifact(artifact)
                .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        }
        self.results
            .lock()
            .map_err(|_| OrchestrationError::Session("fixture source poisoned".into()))?
            .insert((parent.clone(), result.subagent_id.clone()), result.clone());
        Ok(())
    }
    async fn latest(
        &self,
        parent: &SessionId,
        child: &SubagentId,
    ) -> Result<Option<String>, OrchestrationError> {
        Ok(self
            .results
            .lock()
            .map_err(|_| OrchestrationError::Session("fixture source poisoned".into()))?
            .get(&(parent.clone(), child.clone()))
            .and_then(|result| result.diff_artifact.as_ref())
            .map(|artifact| artifact.id.clone()))
    }
}
