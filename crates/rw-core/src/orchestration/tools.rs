use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use rw_ext::AgentRegistry;
use rw_tools::{
    CapabilityManifest, SessionActivity, SubagentEventSink, SubagentLifecycleEvent,
    SubagentLifecycleMode, SubagentProgressEvent, Tool, ToolContext, ToolDescriptor, ToolError,
    ToolResult, WorkspaceBinding,
};
use rw_types::{
    SessionId, SessionMode, SubagentId, SubagentIsolation, SubagentResult, ToolOutput,
    ToolOutputPart,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ModelSource;

use super::{
    ChildDelivery, ChildWait, DeliveryWaiter, OrchestrationError, SubagentHandle, SubagentObserver,
    SubagentOrchestrator, SubagentRequest, SubagentTicket, model_facing_subagent_tool_result,
};

/// Longest single `wait` a model may request.
const MAX_WAIT_SECONDS: u64 = 30 * 60;

const DESCRIPTION: &str = "Delegate work to child agents. Each child is a full agent session \
with its own context that works on one self-contained task. Children run in the background by \
default: `spawn` returns an id at once and you keep working. When a child finishes, its final \
report is delivered to you automatically, exactly once, as a <child-agent-result> message; if \
you are idle, a new turn starts for it. Spawn several children in one response to parallelize \
independent investigations or edits. Use `wait` only when you cannot continue without a result. \
Children in a `worktree` (the default) edit a private copy and return a diff artifact you apply \
with `apply_worktree_diff`; `shared` children work in your workspace.";

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

/// One operation on your child agents, selected by `action`.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum SpawnAgentAction {
    /// Start a child agent on a self-contained task. In the background (the
    /// default) this returns the child's id immediately and its final report is
    /// delivered to you when it finishes. When every child slot is busy the
    /// child is queued and starts as soon as one frees.
    Spawn {
        /// Complete instructions. The child sees only this text, not your conversation,
        /// so include the goal, relevant paths, constraints, and what to report back.
        task: String,
        /// Agent definition to run, such as `general` (edits and runs commands),
        /// `explore` (read-only research), or `plan`. Defaults to `general`.
        #[serde(default = "default_agent")]
        agent: String,
        /// `worktree` (default): a private git worktree whose edits return as a diff
        /// artifact. `shared`: your workspace; an editing child then holds the
        /// workspace lock and your own edits wait until it finishes.
        #[serde(default)]
        isolation: SubagentIsolation,
        /// `true` (default): return immediately and keep working. `false`: block this
        /// call until the child finishes; use it only when you cannot progress without it.
        #[serde(default = "default_background")]
        background: bool,
    },
    /// Block until every listed child finishes or the timeout passes. Use this only
    /// when you need results before you can continue. Reports are still delivered
    /// as <child-agent-result> messages; this call returns each child's status.
    Wait {
        /// Children to wait for.
        #[schemars(length(min = 1, max = 64))]
        ids: Vec<SubagentId>,
        /// Seconds to wait before returning with the children still running (1-1800).
        /// Omit to wait until all of them finish.
        #[serde(default)]
        #[schemars(range(min = 1, max = 1800))]
        timeout_seconds: Option<u64>,
    },
    /// Continue a finished child with a follow-up. It keeps the context of its
    /// earlier work, so this is cheaper than spawning a new child.
    Message {
        /// Child to continue.
        id: SubagentId,
        /// Follow-up instructions for the child.
        message: String,
        /// `true` (default): return immediately; the new report is delivered when it
        /// finishes. `false`: block this call until the child finishes.
        #[serde(default = "default_background")]
        background: bool,
    },
    /// List your children with their agent, task, and state (`queued`, `running`, or `idle`).
    List {},
    /// Stop a running or queued child. A stopped child can still receive a `message`.
    Cancel {
        /// Child to stop.
        id: SubagentId,
    },
    /// Discard a finished child you no longer need, releasing its slot and worktree.
    /// It can no longer receive messages; delivered results and diff artifacts remain.
    Close {
        /// Child to discard.
        id: SubagentId,
    },
}

fn default_agent() -> String {
    "general".to_owned()
}

const fn default_background() -> bool {
    true
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

    /// Only a running child that can edit the shared workspace holds the lock.
    /// Worktree and read-only children never block the parent.
    fn session_activity(&self, session: &rw_types::SessionId) -> Option<SessionActivity> {
        self.orchestrator
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|child| {
                &child.parent_session_id == session
                    && child.state == super::SessionState::Active
                    && child.shares_workspace_writes
            })
            .then_some(SessionActivity::SharedWorkspaceChild)
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_agent".to_owned(),
            description: DESCRIPTION.to_owned(),
            input_schema: serde_json::to_value(schemars::schema_for!(SpawnAgentAction))
                .unwrap_or(Value::Null),
            capabilities: self.capabilities.clone(),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    /// Declared so nested delegation cannot forge child lifecycles. Lifecycle
    /// records always flow through the session-owned sink.
    fn subagent_lifecycle_mode(&self) -> SubagentLifecycleMode {
        SubagentLifecycleMode::Single
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        let Ok(action) = serde_json::from_value::<SpawnAgentAction>(input.clone()) else {
            return false;
        };
        match action {
            SpawnAgentAction::Wait { .. } | SpawnAgentAction::List {} => true,
            SpawnAgentAction::Message { id, .. }
            | SpawnAgentAction::Cancel { id }
            | SpawnAgentAction::Close { id } => self
                .orchestrator
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .is_some_and(|record| !record.shares_workspace_writes),
            SpawnAgentAction::Spawn {
                agent, isolation, ..
            } => {
                isolation == SubagentIsolation::Worktree
                    || self
                        .agents
                        .load(&agent)
                        .is_ok_and(|agent| agent.permission_mode != SessionMode::Execute)
            }
        }
    }

    fn invocation_capabilities(&self, input: &Value) -> Result<CapabilityManifest, ToolError> {
        let action: SpawnAgentAction = serde_json::from_value(input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        if let SpawnAgentAction::Spawn { agent, .. } = action {
            self.agents
                .load(&agent)
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        }
        Ok(self.capabilities.clone())
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let action: SpawnAgentAction = serde_json::from_value(input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let parent = context
            .session_id()
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("spawn_agent requires a session".to_owned()))?;
        match action {
            SpawnAgentAction::List {} => Ok(self.list(&parent)),
            SpawnAgentAction::Wait {
                ids,
                timeout_seconds,
            } => self.wait(context, &parent, ids, timeout_seconds).await,
            SpawnAgentAction::Cancel { id } => {
                self.orchestrator
                    .cancel(&parent, &id)
                    .await
                    .map_err(command)?;
                control(
                    &id,
                    "cancel",
                    "Cancelled child {id}. Its partial report will be delivered; `message` can continue it.",
                )
            }
            SpawnAgentAction::Close { id } => {
                self.orchestrator
                    .close(&parent, &id)
                    .await
                    .map_err(command)?;
                control(
                    &id,
                    "close",
                    "Closed child {id}; its slot and worktree are released.",
                )
            }
            SpawnAgentAction::Spawn {
                task,
                agent,
                isolation,
                background,
            } => {
                self.spawn(context, parent, task, &agent, isolation, background)
                    .await
            }
            SpawnAgentAction::Message {
                id,
                message,
                background,
            } => {
                let delivery = ChildDelivery::new(background);
                let waiter = (!background).then(|| delivery.waiter());
                let observer = self.observer(context, &delivery)?;
                let ticket = self
                    .orchestrator
                    .continue_child(
                        &parent,
                        &id,
                        message,
                        observer,
                        rw_tools::CancellationToken::default(),
                    )
                    .await
                    .map_err(command)?;
                self.settle(context, &parent, &ticket, waiter).await
            }
        }
    }
}

impl SpawnAgentTool {
    fn observer(
        &self,
        context: &ToolContext,
        delivery: &Arc<ChildDelivery>,
    ) -> Result<Arc<dyn SubagentObserver>, ToolError> {
        let events = context
            .background_subagent_event_sink()
            .cloned()
            .ok_or_else(|| {
                ToolError::InvalidInput("spawn_agent requires engine lifecycle routing".into())
            })?;
        Ok(Arc::new(ToolObserver::new(
            events,
            Arc::clone(delivery),
            self.orchestrator.limits().wake_on_completion,
        )))
    }

    async fn spawn(
        &self,
        context: &ToolContext,
        parent: SessionId,
        task: String,
        agent_name: &str,
        isolation: SubagentIsolation,
        background: bool,
    ) -> Result<ToolResult, ToolError> {
        let loaded = self
            .agents
            .load(agent_name)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
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
        let request = SubagentRequest {
            isolation,
            ..SubagentRequest::from_loaded_agent(
                task,
                loaded,
                inherited_model,
                context.workspace_root().to_path_buf(),
            )
        };
        let delivery = ChildDelivery::new(background);
        let waiter = (!background).then(|| delivery.waiter());
        let observer = self.observer(context, &delivery)?;
        // The child outlives this call; a foreground call cancels it explicitly.
        let startup = self.orchestrator.submit(
            parent.clone(),
            request,
            observer,
            rw_tools::CancellationToken::default(),
        );
        let ticket = tokio::select! {
            ticket = startup => ticket.map_err(command)?,
            () = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
        };
        self.settle(context, &parent, &ticket, waiter).await
    }

    /// Returns at once for a background invocation, or waits for a foreground one.
    async fn settle(
        &self,
        context: &ToolContext,
        parent: &SessionId,
        ticket: &SubagentTicket,
        waiter: Option<DeliveryWaiter>,
    ) -> Result<ToolResult, ToolError> {
        let id = &ticket.handle.subagent_id;
        let Some(mut waiter) = waiter else {
            let summary = if ticket.queued {
                format!(
                    "Queued child {}: all {} child slots are busy, so it starts when one frees. Keep working; its final report will be delivered to you automatically.",
                    id.0,
                    self.orchestrator.limits().max_concurrency
                )
            } else {
                format!(
                    "Started child {} in the background. Keep working; its final report will be delivered to you automatically when it finishes.",
                    id.0
                )
            };
            return started(&ticket.handle, ticket.queued, summary);
        };
        // A foreground child belongs to its call: interrupting or dropping the
        // call cancels the child unless it was moved to the background.
        let mut owner = ForegroundOwner {
            orchestrator: Some(self.orchestrator.clone()),
            parent: parent.clone(),
            id: id.clone(),
        };
        let outcome = tokio::select! {
            outcome = self.orchestrator.wait_for_parent(parent, id) => {
                finished_result(id, &outcome.map_err(command)?)
            }
            () = waiter.detached() => moved_to_background(&ticket.handle),
            () = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
        };
        owner.orchestrator = None;
        outcome
    }

    fn list(&self, parent: &SessionId) -> ToolResult {
        let children = self
            .orchestrator
            .list_for_parent(parent)
            .into_iter()
            .map(|child| {
                json!({
                    "id": child.subagent_id,
                    "agent": child.agent,
                    "task": preview(&child.task),
                    "isolation": child.isolation,
                    "state": child.activity,
                })
            })
            .collect::<Vec<_>>();
        let content = if children.is_empty() {
            "You have no child agents.".to_owned()
        } else {
            format!("You have {} child agent(s).", children.len())
        };
        ToolResult::new(content, json!({ "children": children }))
    }

    async fn wait(
        &self,
        context: &ToolContext,
        parent: &SessionId,
        ids: Vec<SubagentId>,
        timeout_seconds: Option<u64>,
    ) -> Result<ToolResult, ToolError> {
        if ids.is_empty() || ids.len() > 64 {
            return Err(ToolError::InvalidInput(
                "wait requires between 1 and 64 child ids".to_owned(),
            ));
        }
        let timeout = match timeout_seconds {
            Some(seconds @ 1..=MAX_WAIT_SECONDS) => Duration::from_secs(seconds),
            Some(_) => {
                return Err(ToolError::InvalidInput(format!(
                    "timeout_seconds must be between 1 and {MAX_WAIT_SECONDS}"
                )));
            }
            None => Duration::from_secs(MAX_WAIT_SECONDS),
        };
        let mut waiters = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(waiter) = self
                .orchestrator
                .foreground_waiter(parent, id)
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?
            {
                waiters.push(waiter);
            }
        }
        let mut outcomes: Vec<Option<Result<ChildWait, OrchestrationError>>> =
            (0..ids.len()).map(|_| None).collect();
        let detached = async {
            let mut detached = waiters
                .iter_mut()
                .map(DeliveryWaiter::detached)
                .collect::<futures_util::stream::FuturesUnordered<_>>();
            if futures_util::StreamExt::next(&mut detached).await.is_none() {
                std::future::pending::<()>().await;
            }
        };
        let all = async {
            let mut pending = futures_util::stream::FuturesUnordered::new();
            for (index, id) in ids.iter().enumerate() {
                let orchestrator = self.orchestrator.clone();
                pending
                    .push(async move { (index, orchestrator.wait_for_parent(parent, id).await) });
            }
            while let Some((index, outcome)) = futures_util::StreamExt::next(&mut pending).await {
                outcomes[index] = Some(outcome);
            }
        };
        let interrupted = tokio::select! {
            () = all => false,
            () = detached => false,
            () = tokio::time::sleep(timeout) => false,
            () = context.cancellation.cancelled() => true,
        };
        if interrupted {
            return Err(ToolError::Command(
                "wait interrupted; the children keep running and their reports will be delivered"
                    .into(),
            ));
        }
        wait_result(&ids, outcomes)
    }
}

/// Cancels a foreground child when its owning call ends without a result.
struct ForegroundOwner {
    orchestrator: Option<SubagentOrchestrator>,
    parent: SessionId,
    id: SubagentId,
}

impl Drop for ForegroundOwner {
    fn drop(&mut self) {
        if let Some(orchestrator) = self.orchestrator.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let (parent, id) = (self.parent.clone(), self.id.clone());
            runtime.spawn(async move {
                let _ = orchestrator.cancel(&parent, &id).await;
            });
        }
    }
}

#[allow(clippy::needless_pass_by_value)] // Shaped for `map_err`.
fn command(error: OrchestrationError) -> ToolError {
    ToolError::Command(error.to_string())
}

fn preview(task: &str) -> String {
    let mut preview = task.to_owned();
    super::policy::truncate_utf8(&mut preview, 200);
    preview
}

fn control(id: &SubagentId, action: &str, template: &str) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::new(
        template.replace("{id}", &id.0),
        json!({ "id": id, "action": action, "completed": true }),
    )
    .with_presentation(super::presentation::CONTROL.plan()?))
}

fn started(
    handle: &SubagentHandle,
    queued: bool,
    content: String,
) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::new(
        content,
        json!({
            "id": handle.subagent_id,
            "session_id": handle.session_id,
            "status": if queued { "queued" } else { "running" },
            "background": true,
        }),
    )
    .with_presentation(super::presentation::STATUS.plan()?))
}

fn moved_to_background(handle: &SubagentHandle) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::new(
        format!(
            "Child {} was moved to the background. Continue with other work; its final report will be delivered to you automatically when it completes.",
            handle.subagent_id.0
        ),
        json!({
            "id": handle.subagent_id,
            "session_id": handle.session_id,
            "status": "running",
            "background": true,
        }),
    )
    .with_presentation(super::presentation::STATUS.plan()?))
}

fn finished_result(id: &SubagentId, outcome: &ChildWait) -> Result<ToolResult, ToolError> {
    let (content, status) = match outcome {
        ChildWait::Finished(result) => (
            format!(
                "Child {} finished with status {}. Its final report follows as a <child-agent-result id=\"{}\"> message.",
                id.0,
                status_name(result),
                id.0
            ),
            status_name(result).to_owned(),
        ),
        ChildWait::NeverStarted(reason) => (
            format!("Child {} did not run: {reason}.", id.0),
            "not_started".to_owned(),
        ),
    };
    Ok(ToolResult::new(
        content,
        json!({ "id": id, "status": status, "background": false }),
    )
    .with_presentation(super::presentation::STATUS.plan()?))
}

fn wait_result(
    ids: &[SubagentId],
    outcomes: Vec<Option<Result<ChildWait, OrchestrationError>>>,
) -> Result<ToolResult, ToolError> {
    let mut lines = Vec::with_capacity(ids.len());
    let mut children = Vec::with_capacity(ids.len());
    for (id, outcome) in ids.iter().zip(outcomes) {
        let (status, line) = match outcome {
            None => (
                "running".to_owned(),
                format!(
                    "{}: still running; its report will be delivered when it finishes",
                    id.0
                ),
            ),
            Some(Ok(ChildWait::Finished(result))) => (
                status_name(&result).to_owned(),
                format!(
                    "{}: {}; its report is delivered as a <child-agent-result> message",
                    id.0,
                    status_name(&result)
                ),
            ),
            Some(Ok(ChildWait::NeverStarted(reason))) => (
                "not_started".to_owned(),
                format!("{}: did not run: {reason}", id.0),
            ),
            Some(Err(error)) => ("unavailable".to_owned(), format!("{}: {error}", id.0)),
        };
        lines.push(line);
        children.push(json!({ "id": id, "status": status }));
    }
    Ok(
        ToolResult::new(lines.join("\n"), json!({ "children": children }))
            .with_presentation(super::presentation::WAIT.plan()?),
    )
}

fn status_name(result: &SubagentResult) -> &'static str {
    match result.status {
        rw_types::SubagentStatus::Completed => "completed",
        rw_types::SubagentStatus::Failed => "failed",
        rw_types::SubagentStatus::Cancelled => "cancelled",
        rw_types::SubagentStatus::TimedOut => "timed_out",
        rw_types::SubagentStatus::MaxTurns => "max_turns",
    }
}

pub(super) struct ToolObserver {
    events: Arc<dyn SubagentEventSink>,
    delivery: Arc<ChildDelivery>,
    wake_on_completion: bool,
}

impl ToolObserver {
    pub(super) fn new(
        events: Arc<dyn SubagentEventSink>,
        delivery: Arc<ChildDelivery>,
        wake_on_completion: bool,
    ) -> Self {
        Self {
            events,
            delivery,
            wake_on_completion,
        }
    }
}

#[async_trait]
impl SubagentObserver for ToolObserver {
    fn progress_budget(&self) -> rw_tools::ChildProgressBudget {
        self.events.progress_budget()
    }

    fn delivery(&self) -> Option<Arc<ChildDelivery>> {
        Some(Arc::clone(&self.delivery))
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
        let event = SubagentLifecycleEvent::Finished {
            subagent_id: result.subagent_id.clone(),
            result: Box::new(result.clone()),
        };
        if self.wake_on_completion && self.delivery.is_background() {
            self.events.background_finished(event).await
        } else {
            self.events.lifecycle(event).await
        }
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
