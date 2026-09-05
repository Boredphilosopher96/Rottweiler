use std::sync::Arc;

use async_trait::async_trait;
use rw_ext::AgentRegistry;
use rw_tools::{
    CapabilityManifest, SubagentEventSink, SubagentLifecycleEvent, SubagentLifecycleMode,
    SubagentProgressEvent, Tool, ToolContext, ToolDescriptor, ToolError, ToolResult,
    WorkspaceBinding,
};
use rw_types::{
    SessionMode, SubagentId, SubagentIsolation, SubagentResult, ToolOutput, ToolOutputPart,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ModelDriver;

use super::{
    MAX_SUBAGENT_PROGRESS_BYTES, OrchestrationError, SubagentHandle, SubagentObserver,
    SubagentOrchestrator, SubagentRequest, model_facing_subagent_tool_result,
};

/// Public registry tool. Depth is derived from the parent session handle, never model input.
pub struct SpawnAgentTool {
    orchestrator: SubagentOrchestrator,
    agents: Arc<AgentRegistry>,
    model: Arc<dyn ModelDriver>,
    capabilities: CapabilityManifest,
}

impl SpawnAgentTool {
    #[must_use]
    pub fn new(
        orchestrator: SubagentOrchestrator,
        agents: Arc<AgentRegistry>,
        model: Arc<dyn ModelDriver>,
    ) -> Self {
        // Spawning, resuming, interrupting, and closing a child are control-plane
        // operations. They do not exercise the child's tool authority. The child
        // receives a fork of the parent's effective permission gate and each tool
        // call is authorized there, exactly once.
        let capabilities = CapabilityManifest::default();
        Self {
            orchestrator,
            agents,
            model,
            capabilities,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SpawnAgentInput {
    #[serde(default)]
    action: Option<SpawnAgentAction>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    isolation: Option<SubagentIsolation>,
    #[serde(default)]
    subagent_id: Option<SubagentId>,
    #[serde(default)]
    follow_up: Option<String>,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum SpawnAgentAction {
    Spawn,
    FollowUp,
    Cancel,
    Close,
}

pub(super) enum NormalizedSpawnAgentAction {
    Spawn {
        task: String,
        agent: String,
        isolation: SubagentIsolation,
    },
    FollowUp {
        subagent_id: SubagentId,
        prompt: String,
    },
    Cancel {
        subagent_id: SubagentId,
    },
    Close {
        subagent_id: SubagentId,
    },
}

pub(super) fn normalize_spawn_agent_input(
    input: SpawnAgentInput,
) -> Result<NormalizedSpawnAgentAction, ToolError> {
    let action = input.action.unwrap_or_else(|| {
        if input.subagent_id.is_some() && input.follow_up.is_some() {
            SpawnAgentAction::FollowUp
        } else {
            SpawnAgentAction::Spawn
        }
    });
    let invalid = |message: &str| ToolError::InvalidInput(message.to_owned());
    match action {
        SpawnAgentAction::Spawn => {
            if input.subagent_id.is_some() || input.follow_up.is_some() {
                return Err(invalid("spawn forbids subagent_id and follow_up"));
            }
            let task = input.task.ok_or_else(|| invalid("spawn requires task"))?;
            Ok(NormalizedSpawnAgentAction::Spawn {
                task,
                agent: input.agent.unwrap_or_else(|| "general".to_owned()),
                isolation: input.isolation.unwrap_or_default(),
            })
        }
        SpawnAgentAction::FollowUp => {
            if input.task.is_some() || input.agent.is_some() || input.isolation.is_some() {
                return Err(invalid("follow_up forbids task, agent, and isolation"));
            }
            Ok(NormalizedSpawnAgentAction::FollowUp {
                subagent_id: input
                    .subagent_id
                    .ok_or_else(|| invalid("follow_up requires subagent_id"))?,
                prompt: input
                    .follow_up
                    .ok_or_else(|| invalid("follow_up requires a prompt"))?,
            })
        }
        SpawnAgentAction::Cancel | SpawnAgentAction::Close => {
            if input.task.is_some()
                || input.agent.is_some()
                || input.isolation.is_some()
                || input.follow_up.is_some()
            {
                return Err(invalid("cancel/close accepts only action and subagent_id"));
            }
            let subagent_id = input
                .subagent_id
                .ok_or_else(|| invalid("cancel/close requires subagent_id"))?;
            Ok(match action {
                SpawnAgentAction::Cancel => NormalizedSpawnAgentAction::Cancel { subagent_id },
                SpawnAgentAction::Close => NormalizedSpawnAgentAction::Close { subagent_id },
                SpawnAgentAction::Spawn | SpawnAgentAction::FollowUp => unreachable!(),
            })
        }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_agent".to_owned(),
            description: "Spawn a restricted full child session, or continue a completed child"
                .to_owned(),
            input_schema: serde_json::to_value(schemars::schema_for!(SpawnAgentInput))
                .unwrap_or(Value::Null),
            capabilities: self.capabilities.clone(),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    fn subagent_lifecycle_mode(&self) -> SubagentLifecycleMode {
        SubagentLifecycleMode::Single
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        let Ok(input) = serde_json::from_value::<SpawnAgentInput>(input.clone()) else {
            return false;
        };
        let Ok(action) = normalize_spawn_agent_input(input) else {
            return false;
        };
        match action {
            NormalizedSpawnAgentAction::FollowUp { subagent_id, .. }
            | NormalizedSpawnAgentAction::Cancel { subagent_id }
            | NormalizedSpawnAgentAction::Close { subagent_id } => self
                .orchestrator
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&subagent_id)
                .is_some_and(|record| record.isolation == SubagentIsolation::Worktree),
            NormalizedSpawnAgentAction::Spawn {
                agent, isolation, ..
            } => {
                if isolation == SubagentIsolation::Worktree {
                    return true;
                }
                self.agents
                    .load(&agent)
                    .is_ok_and(|agent| agent.permission_mode != SessionMode::Execute)
            }
        }
    }

    fn invocation_capabilities(&self, input: &Value) -> Result<CapabilityManifest, ToolError> {
        let input: SpawnAgentInput = serde_json::from_value(input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let action = normalize_spawn_agent_input(input)?;
        match action {
            NormalizedSpawnAgentAction::FollowUp { subagent_id, .. } => {
                self.orchestrator
                    .inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&subagent_id)
                    .ok_or_else(|| ToolError::InvalidInput("unknown child session".to_owned()))?;
                Ok(CapabilityManifest::default())
            }
            NormalizedSpawnAgentAction::Cancel { .. }
            | NormalizedSpawnAgentAction::Close { .. } => Ok(self.capabilities.clone()),
            NormalizedSpawnAgentAction::Spawn { agent, .. } => {
                self.agents
                    .load(&agent)
                    .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
                Ok(CapabilityManifest::default())
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let input: SpawnAgentInput = serde_json::from_value(input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let action = normalize_spawn_agent_input(input)?;
        let parent_session_id = context
            .session_id()
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("spawn_agent requires a session".to_owned()))?;
        let events = context.subagent_event_sink().cloned().ok_or_else(|| {
            ToolError::InvalidInput("spawn_agent requires engine lifecycle routing".to_owned())
        })?;
        let observer: Arc<dyn SubagentObserver> = Arc::new(ToolObserver { events });
        if let NormalizedSpawnAgentAction::Cancel { subagent_id }
        | NormalizedSpawnAgentAction::Close { subagent_id } = &action
        {
            match &action {
                NormalizedSpawnAgentAction::Cancel { .. } => self
                    .orchestrator
                    .cancel(&parent_session_id, subagent_id)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?,
                NormalizedSpawnAgentAction::Close { .. } => self
                    .orchestrator
                    .close(&parent_session_id, subagent_id)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?,
                NormalizedSpawnAgentAction::Spawn { .. }
                | NormalizedSpawnAgentAction::FollowUp { .. } => unreachable!(),
            }
            return Ok(ToolResult::new(
                format!("subagent {} action completed", subagent_id.0),
                json!({
                    "subagent_id": subagent_id,
                    "action": match action {
                        NormalizedSpawnAgentAction::Cancel { .. } => "cancel",
                        NormalizedSpawnAgentAction::Close { .. } => "close",
                        NormalizedSpawnAgentAction::Spawn { .. }
                        | NormalizedSpawnAgentAction::FollowUp { .. } => unreachable!(),
                    },
                    "completed": true,
                }),
            ));
        }
        let result = match action {
            NormalizedSpawnAgentAction::FollowUp {
                subagent_id,
                prompt,
            } => {
                let handle = self
                    .orchestrator
                    .follow_up(
                        &parent_session_id,
                        &subagent_id,
                        prompt,
                        observer,
                        context.cancellation.clone(),
                    )
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?;
                self.orchestrator
                    .wait(&handle)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?
            }
            NormalizedSpawnAgentAction::Spawn {
                task,
                agent: agent_name,
                isolation,
            } => {
                let loaded = self
                    .agents
                    .load(&agent_name)
                    .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
                let inherited_model = context.model_alias().ok_or_else(|| {
                    ToolError::InvalidInput(
                        "spawn_agent requires the parent turn's selected model".to_owned(),
                    )
                })?;
                let resolved_model = loaded.model.as_deref().unwrap_or(inherited_model);
                if !self.model.has_model_alias(resolved_model) {
                    return Err(ToolError::InvalidInput(format!(
                        "agent `{agent_name}` selects unconfigured model alias `{resolved_model}`"
                    )));
                }
                let request = SubagentRequest::from_loaded_agent(
                    task,
                    loaded,
                    inherited_model,
                    context.workspace_root().to_path_buf(),
                );
                let request = SubagentRequest {
                    isolation,
                    ..request
                };
                self.orchestrator
                    .spawn(
                        parent_session_id,
                        request,
                        observer,
                        context.cancellation.clone(),
                    )
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?
            }
            NormalizedSpawnAgentAction::Cancel { .. }
            | NormalizedSpawnAgentAction::Close { .. } => unreachable!(),
        };
        Ok(model_facing_subagent_tool_result(&result))
    }
}

pub(super) struct ToolObserver {
    events: Arc<dyn SubagentEventSink>,
}

#[async_trait]
impl SubagentObserver for ToolObserver {
    async fn spawned(&self, handle: &SubagentHandle, task: &str) -> Result<(), OrchestrationError> {
        self.events
            .lifecycle(SubagentLifecycleEvent::Spawned {
                subagent_id: handle.subagent_id.clone(),
                child_session_id: handle.session_id.clone(),
                task: task.to_owned(),
            })
            .await
            .map_err(|error| OrchestrationError::Observer(error.to_string()))
    }

    async fn finished(&self, result: &SubagentResult) -> Result<(), OrchestrationError> {
        self.events
            .lifecycle(SubagentLifecycleEvent::Finished {
                subagent_id: result.subagent_id.clone(),
                result: Box::new(result.clone()),
            })
            .await
            .map_err(|error| OrchestrationError::Observer(error.to_string()))
    }

    async fn progress(
        &self,
        handle: &SubagentHandle,
        child_sequence: Option<u64>,
        event: Value,
    ) -> Result<(), OrchestrationError> {
        if serde_json::to_vec(&event)
            .is_ok_and(|encoded| encoded.len() > MAX_SUBAGENT_PROGRESS_BYTES)
        {
            return Err(OrchestrationError::Observer(
                "child progress event exceeds size limit".to_owned(),
            ));
        }
        self.events
            .progress(SubagentProgressEvent {
                subagent_id: handle.subagent_id.clone(),
                child_session_id: handle.session_id.clone(),
                child_sequence,
                event,
            })
            .await
            .map_err(|error| OrchestrationError::Observer(error.to_string()))
    }
}

/// Canonical complete child result used by every durable lifecycle bridge.
#[must_use]
pub fn subagent_result_tool_output(result: &SubagentResult) -> ToolOutput {
    let bounded = model_facing_subagent_tool_result(result);
    ToolOutput::Mixed {
        parts: vec![
            ToolOutputPart::Text {
                text: bounded.content,
            },
            ToolOutputPart::Structured {
                value: bounded.data,
            },
        ],
    }
}
