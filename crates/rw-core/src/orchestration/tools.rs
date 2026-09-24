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

use crate::ModelSource;

use super::{
    OrchestrationError, SubagentHandle, SubagentObserver, SubagentOrchestrator, SubagentRequest,
    model_facing_subagent_tool_result,
};

/// Public registry tool. Depth is derived from the parent session handle, never model input.
pub struct SpawnAgentTool {
    orchestrator: SubagentOrchestrator,
    agents: Arc<AgentRegistry>,
    model: Arc<dyn ModelSource>,
    capabilities: CapabilityManifest,
}

impl SpawnAgentTool {
    #[must_use]
    pub fn new(
        orchestrator: SubagentOrchestrator,
        agents: Arc<AgentRegistry>,
        model: Arc<dyn ModelSource>,
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
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum SpawnAgentAction {
    Start {
        task: String,
        #[serde(default = "default_agent")]
        agent: String,
        #[serde(default)]
        isolation: SubagentIsolation,
    },
    Wait {
        subagent_id: SubagentId,
    },
    List {},
    Spawn {
        task: String,
        #[serde(default = "default_agent")]
        agent: String,
        #[serde(default)]
        isolation: SubagentIsolation,
    },
    FollowUp {
        subagent_id: SubagentId,
        #[serde(rename = "follow_up")]
        prompt: String,
    },
    Cancel {
        subagent_id: SubagentId,
    },
    Close {
        subagent_id: SubagentId,
    },
}

fn default_agent() -> String {
    "general".to_owned()
}

#[async_trait]
impl Tool for SpawnAgentTool {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        self.orchestrator
            .settle_startups()
            .await
            .map_err(|error| ToolError::EffectsUnsettled(error.to_string()))
    }

    async fn end_session(&self, session: &rw_types::SessionId) -> Result<(), ToolError> {
        let startups = self.orchestrator.settle_startups().await;
        let children = self.orchestrator.suspend_parent(session).await;
        startups
            .and(children)
            .map_err(|error| ToolError::EffectsUnsettled(error.to_string()))
    }

    fn session_activity(&self, session: &rw_types::SessionId) -> Option<String> {
        self.orchestrator
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|child| {
                &child.parent_session_id == session && child.state == super::SessionState::Active
            })
            .then(|| "A child agent is still running; wait for it or interrupt it first.".into())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_agent".to_owned(),
            description: "Start a child in the background; list children and wait for results. Spawn waits immediately; follow_up continues a completed child"
                .to_owned(),
            input_schema: serde_json::to_value(schemars::schema_for!(SpawnAgentAction))
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
        let Ok(action) = serde_json::from_value::<SpawnAgentAction>(input.clone()) else {
            return false;
        };
        match action {
            SpawnAgentAction::Wait { .. } | SpawnAgentAction::List {} => true,
            SpawnAgentAction::FollowUp { subagent_id, .. }
            | SpawnAgentAction::Cancel { subagent_id }
            | SpawnAgentAction::Close { subagent_id } => self
                .orchestrator
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&subagent_id)
                .is_some_and(|record| record.isolation == SubagentIsolation::Worktree),
            SpawnAgentAction::Spawn {
                agent, isolation, ..
            }
            | SpawnAgentAction::Start {
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
        let action: SpawnAgentAction = serde_json::from_value(input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        match action {
            SpawnAgentAction::FollowUp { subagent_id, .. } => {
                self.orchestrator
                    .inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&subagent_id)
                    .ok_or_else(|| ToolError::InvalidInput("unknown child session".to_owned()))?;
                Ok(CapabilityManifest::default())
            }
            SpawnAgentAction::Cancel { .. }
            | SpawnAgentAction::Close { .. }
            | SpawnAgentAction::Wait { .. }
            | SpawnAgentAction::List {} => Ok(self.capabilities.clone()),
            SpawnAgentAction::Spawn { agent, .. } | SpawnAgentAction::Start { agent, .. } => {
                self.agents
                    .load(&agent)
                    .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
                Ok(CapabilityManifest::default())
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let action: SpawnAgentAction = serde_json::from_value(input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let parent_session_id = context
            .session_id()
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("spawn_agent requires a session".to_owned()))?;
        if let SpawnAgentAction::List {} = action {
            let children = self
                .orchestrator
                .list_for_parent(&parent_session_id)
                .into_iter()
                .map(|child| {
                    json!({
                        "subagent_id": child.subagent_id,
                        "session_id": child.child_session_id,
                        "activity": child.activity,
                    })
                })
                .collect::<Vec<_>>();
            return Ok(ToolResult::new(
                "Child agent status",
                json!({ "children": children }),
            ));
        }
        if let SpawnAgentAction::Wait { subagent_id } = &action {
            let child = self
                .orchestrator
                .descriptor_for_parent(&parent_session_id, subagent_id)
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
            let handle = SubagentHandle {
                subagent_id: child.subagent_id,
                session_id: child.child_session_id,
            };
            let result = tokio::select! {
                result = self.orchestrator.wait(&handle) => result.map_err(|error| ToolError::Command(error.to_string()))?,
                () = context.cancellation.cancelled() => return Err(ToolError::Command("child wait interrupted; the background child remains available".into())),
            };
            return Ok(model_facing_subagent_tool_result(&result)
                .with_presentation(super::presentation::RESULT.plan()?));
        }
        let background = matches!(&action, SpawnAgentAction::Start { .. });
        let events = if background {
            context.background_subagent_event_sink()
        } else {
            context.subagent_event_sink()
        }
        .cloned()
        .ok_or_else(|| {
            ToolError::InvalidInput("spawn_agent requires engine lifecycle routing".into())
        })?;
        let observer: Arc<dyn SubagentObserver> = Arc::new(ToolObserver { events });
        if let SpawnAgentAction::Cancel { subagent_id } | SpawnAgentAction::Close { subagent_id } =
            &action
        {
            match &action {
                SpawnAgentAction::Cancel { .. } => self
                    .orchestrator
                    .cancel(&parent_session_id, subagent_id)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?,
                SpawnAgentAction::Close { .. } => self
                    .orchestrator
                    .close(&parent_session_id, subagent_id)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?,
                SpawnAgentAction::Spawn { .. }
                | SpawnAgentAction::Start { .. }
                | SpawnAgentAction::Wait { .. }
                | SpawnAgentAction::List {}
                | SpawnAgentAction::FollowUp { .. } => {
                    unreachable!()
                }
            }
            return Ok(ToolResult::new(
                format!("subagent {} action completed", subagent_id.0),
                json!({
                    "subagent_id": subagent_id,
                    "action": match action {
                        SpawnAgentAction::Cancel { .. } => "cancel",
                        SpawnAgentAction::Close { .. } => "close",
                        SpawnAgentAction::Spawn { .. } | SpawnAgentAction::Start { .. }
                        | SpawnAgentAction::Wait { .. } | SpawnAgentAction::List {}
                        | SpawnAgentAction::FollowUp { .. } => unreachable!(),
                    },
                    "completed": true,
                }),
            )
            .with_presentation(super::presentation::CONTROL.plan()?));
        }
        let result = match action {
            SpawnAgentAction::FollowUp {
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
            SpawnAgentAction::Spawn {
                task,
                agent: agent_name,
                isolation,
            }
            | SpawnAgentAction::Start {
                task,
                agent: agent_name,
                isolation,
            } => {
                let loaded = self
                    .agents
                    .load(&agent_name)
                    .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
                if background
                    && isolation == SubagentIsolation::Shared
                    && loaded.permission_mode == SessionMode::Execute
                {
                    return Err(ToolError::InvalidInput(
                        "background execution requires worktree isolation; shared background children must use a read-only agent".into(),
                    ));
                }
                let inherited_model = context.model_alias().ok_or_else(|| {
                    ToolError::InvalidInput(
                        "spawn_agent requires the parent turn's selected model".to_owned(),
                    )
                })?;
                let resolved_model = loaded.model.as_deref().unwrap_or(inherited_model);
                let model = self
                    .model
                    .resolve()
                    .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
                if !model.has_model_alias(resolved_model) {
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
                let cancellation = if background {
                    rw_tools::CancellationToken::default()
                } else {
                    context.cancellation.clone()
                };
                let startup =
                    self.orchestrator
                        .start(parent_session_id, request, observer, cancellation);
                let handle = if background {
                    tokio::select! {
                        result = startup => result.map_err(|error| ToolError::Command(error.to_string()))?,
                        () = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
                    }
                } else {
                    startup
                        .await
                        .map_err(|error| ToolError::Command(error.to_string()))?
                };
                if background {
                    return Ok(ToolResult::new(
                        format!("Child {} started. Continue your work; use list to check status and wait to receive its result.", handle.subagent_id.0),
                        json!({ "subagent_id": handle.subagent_id, "session_id": handle.session_id, "action": "start", "completed": false }),
                    ).with_presentation(super::presentation::CONTROL.plan()?));
                }
                self.orchestrator
                    .wait(&handle)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?
            }
            SpawnAgentAction::Cancel { .. }
            | SpawnAgentAction::Close { .. }
            | SpawnAgentAction::Wait { .. }
            | SpawnAgentAction::List {} => unreachable!(),
        };
        Ok(model_facing_subagent_tool_result(&result)
            .with_presentation(super::presentation::RESULT.plan()?))
    }
}

pub(super) struct ToolObserver {
    events: Arc<dyn SubagentEventSink>,
}

#[async_trait]
impl SubagentObserver for ToolObserver {
    fn progress_budget(&self) -> rw_tools::ChildProgressBudget {
        self.events.progress_budget()
    }
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
        event: rw_tools::ChildProgressPreview,
    ) -> Result<(), OrchestrationError> {
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
