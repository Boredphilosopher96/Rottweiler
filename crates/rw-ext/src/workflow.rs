use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
pub use rw_types::workflow::WorkflowStepArtifact;
use rw_types::workflow::{
    MAX_WORKFLOW_EDGES, MAX_WORKFLOW_STEPS, TaskId, WorkflowChild, WorkflowRunState,
    WorkflowTaskOutcome, valid_workflow_name as valid_name,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::discovery::{
    ArtifactLocation, ArtifactOrigin, ArtifactScope, ExtensionDiscoveryError,
    read_bounded_relative_utf8,
};

const MAX_WORKFLOW_BYTES: u64 = 1024 * 1024;

/// Executable reference selected by one workflow step.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum WorkflowStepTarget {
    Agent(String),
    Command(String),
}

/// Failure policy for a workflow step.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum WorkflowOnFail {
    Stop,
    Continue,
}

/// Minimal deterministic condition evaluated from a completed dependency.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum WorkflowCondition {
    Always,
    Success(String),
    Failure(String),
}

/// One validated node in a declarative workflow DAG.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowStep {
    id: String,
    target: WorkflowStepTarget,
    prompt: String,
    needs: Vec<String>,
    inputs: Vec<String>,
    parallel: bool,
    on_fail: WorkflowOnFail,
    condition: WorkflowCondition,
}

impl WorkflowStep {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn target(&self) -> &WorkflowStepTarget {
        &self.target
    }

    #[must_use]
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    #[must_use]
    pub fn needs(&self) -> &[String] {
        &self.needs
    }

    #[must_use]
    pub fn inputs(&self) -> &[String] {
        &self.inputs
    }

    #[must_use]
    pub const fn parallel(&self) -> bool {
        self.parallel
    }

    #[must_use]
    pub const fn on_fail(&self) -> WorkflowOnFail {
        self.on_fail
    }

    #[must_use]
    pub const fn condition(&self) -> &WorkflowCondition {
        &self.condition
    }
}

/// A trust-gated workflow discovered in ADR-014 order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredWorkflow {
    name: String,
    description: String,
    steps: Vec<WorkflowStep>,
    origin: ArtifactOrigin,
}

impl DiscoveredWorkflow {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn steps(&self) -> &[WorkflowStep] {
        &self.steps
    }

    /// Digest of every scheduler field; a changed definition cannot resume an old run.
    ///
    /// # Errors
    /// Returns an error if the validated definition cannot be serialized.
    pub fn definition_digest(&self) -> Result<String, WorkflowRunError> {
        let bytes = serde_json::to_vec(&(&self.name, &self.steps))
            .map_err(|error| WorkflowRunError::Persistence(error.to_string()))?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    #[must_use]
    pub const fn origin(&self) -> &ArtifactOrigin {
        &self.origin
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct WorkflowFile {
    name: Option<String>,
    description: String,
    step: Vec<WorkflowStepFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct WorkflowStepFile {
    id: String,
    agent: Option<String>,
    command: Option<String>,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    parallel: bool,
    #[serde(default)]
    on_fail: RawOnFail,
    #[serde(rename = "if")]
    condition: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawOnFail {
    #[default]
    Stop,
    Continue,
}

pub(crate) fn discover_workflow(
    scope: ArtifactScope,
    location: ArtifactLocation,
    source_root: &std::path::Path,
    path: &std::path::Path,
) -> Result<DiscoveredWorkflow, ExtensionDiscoveryError> {
    let relative =
        path.strip_prefix(source_root)
            .map_err(|_| ExtensionDiscoveryError::InvalidWorkflow {
                path: path.to_owned(),
                message: "workflow escaped its discovery root".to_owned(),
            })?;
    let contents = read_bounded_relative_utf8(source_root, relative, MAX_WORKFLOW_BYTES)?;
    let raw = toml::from_str::<WorkflowFile>(&contents).map_err(|error| {
        ExtensionDiscoveryError::InvalidWorkflow {
            path: path.to_owned(),
            message: error.message().to_owned(),
        }
    })?;
    let file_name = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| ExtensionDiscoveryError::InvalidWorkflow {
            path: path.to_owned(),
            message: "workflow file name must be portable UTF-8".to_owned(),
        })?;
    let name = raw.name.unwrap_or_else(|| file_name.to_owned());
    if name != file_name || !valid_name(&name) {
        return invalid_workflow(path, "`name` must match the canonical workflow file name");
    }
    if raw.description.trim().is_empty() {
        return invalid_workflow(path, "`description` must not be empty");
    }
    if raw.step.is_empty() || raw.step.len() > MAX_WORKFLOW_STEPS {
        return invalid_workflow(path, "workflow must contain between 1 and 64 steps");
    }
    let mut ids = BTreeSet::new();
    let mut steps = Vec::with_capacity(raw.step.len());
    let mut edges = 0_usize;
    for step in raw.step {
        if !valid_name(&step.id) || !ids.insert(step.id.clone()) {
            return invalid_workflow(path, "step ids must be unique canonical names");
        }
        let target = match (step.agent, step.command) {
            (Some(agent), None) if valid_name(&agent) => WorkflowStepTarget::Agent(agent),
            (None, Some(command)) if valid_name(&command) => WorkflowStepTarget::Command(command),
            _ => {
                return invalid_workflow(
                    path,
                    "each step must select exactly one canonical `agent` or `command`",
                );
            }
        };
        edges = edges.saturating_add(step.needs.len());
        let needs = deduplicated(step.needs);
        let inputs = if step.inputs.is_empty() {
            needs.clone()
        } else {
            deduplicated(step.inputs)
        };
        let condition = parse_condition(path, step.condition.as_deref())?;
        steps.push(WorkflowStep {
            id: step.id,
            target,
            prompt: step.prompt,
            needs,
            inputs,
            parallel: step.parallel,
            on_fail: match step.on_fail {
                RawOnFail::Stop => WorkflowOnFail::Stop,
                RawOnFail::Continue => WorkflowOnFail::Continue,
            },
            condition,
        });
    }
    if edges > MAX_WORKFLOW_EDGES {
        return invalid_workflow(path, "workflow exceeds the 256-edge limit");
    }
    validate_graph(path, &steps)?;
    Ok(DiscoveredWorkflow {
        name,
        description: raw.description,
        steps,
        origin: ArtifactOrigin::new(scope, location, path.to_owned()),
    })
}

fn validate_graph(
    path: &std::path::Path,
    steps: &[WorkflowStep],
) -> Result<(), ExtensionDiscoveryError> {
    let ids = steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<BTreeSet<_>>();
    for step in steps {
        if step
            .needs
            .iter()
            .any(|need| need == &step.id || !ids.contains(need.as_str()))
        {
            return invalid_workflow(path, "step dependency is unknown or self-referential");
        }
        if step.inputs.iter().any(|input| !step.needs.contains(input)) {
            return invalid_workflow(path, "artifact `inputs` must also appear in `needs`");
        }
        let condition_dependency = match &step.condition {
            WorkflowCondition::Always => None,
            WorkflowCondition::Success(dependency) | WorkflowCondition::Failure(dependency) => {
                Some(dependency)
            }
        };
        if condition_dependency.is_some_and(|dependency| !step.needs.contains(dependency)) {
            return invalid_workflow(
                path,
                "workflow condition must reference a declared dependency",
            );
        }
    }
    let mut complete = BTreeSet::new();
    while complete.len() != steps.len() {
        let before = complete.len();
        for step in steps {
            if step.needs.iter().all(|need| complete.contains(need)) {
                complete.insert(step.id.clone());
            }
        }
        if complete.len() == before {
            return invalid_workflow(path, "workflow graph contains a cycle");
        }
    }
    Ok(())
}

fn parse_condition(
    path: &std::path::Path,
    condition: Option<&str>,
) -> Result<WorkflowCondition, ExtensionDiscoveryError> {
    let Some(condition) = condition else {
        return Ok(WorkflowCondition::Always);
    };
    if let Some(step) = condition
        .strip_prefix("success:")
        .filter(|step| valid_name(step))
    {
        return Ok(WorkflowCondition::Success(step.to_owned()));
    }
    if let Some(step) = condition
        .strip_prefix("failure:")
        .filter(|step| valid_name(step))
    {
        return Ok(WorkflowCondition::Failure(step.to_owned()));
    }
    invalid_workflow(
        path,
        "`if` must use `success:<dependency>` or `failure:<dependency>`",
    )
}

fn deduplicated(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn invalid_workflow<T>(
    path: &std::path::Path,
    message: &str,
) -> Result<T, ExtensionDiscoveryError> {
    Err(ExtensionDiscoveryError::InvalidWorkflow {
        path: path.to_owned(),
        message: message.to_owned(),
    })
}

/// Bounded artifacts supplied to one workflow node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowStepRequest {
    pub task_id: TaskId,
    pub workflow: String,
    /// Stable contiguous position among nodes that actually execute.
    pub step_index: usize,
    pub step_id: String,
    pub target: WorkflowStepTarget,
    pub prompt: String,
    pub artifacts: BTreeMap<String, Arc<WorkflowStepArtifact>>,
}

/// Public orchestration boundary used by both interactive and headless runners.
#[async_trait]
pub trait WorkflowStepExecutor: Send + Sync {
    async fn execute_step(
        &self,
        request: WorkflowStepRequest,
    ) -> Result<WorkflowStepArtifact, WorkflowStepExecutionError>;
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WorkflowStepExecutionError {
    #[error("{message}")]
    Failed { message: String },
    #[error("{message}")]
    Unsettled { message: String },
}

impl WorkflowStepExecutionError {
    /// Use only when no effect started, or all child effects and cleanup settled.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }
    /// Retain the started obligation when execution or cleanup is unproven.
    #[must_use]
    pub fn unsettled(message: impl Into<String>) -> Self {
        Self::Unsettled {
            message: message.into(),
        }
    }
}

/// Stable status for one completed workflow node.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowStepReport {
    pub id: String,
    pub output: Option<Arc<WorkflowStepArtifact>>,
    pub error: Option<String>,
    pub skipped: bool,
}

/// Successful workflow result in definition order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowRunReport {
    pub workflow: String,
    pub steps: Vec<WorkflowStepReport>,
}

mod runner;
pub use runner::WorkflowRunner;

/// Required durable transitions. Implementations retain write ownership on cancellation.
#[async_trait]
pub trait WorkflowJournal: Send + Sync {
    async fn state(&self) -> Result<WorkflowRunState, WorkflowRunError>;
    async fn claim(&self, tasks: Vec<TaskId>) -> Result<(), WorkflowRunError>;
    async fn bind_child(&self, task: TaskId, child: WorkflowChild) -> Result<(), WorkflowRunError>;
    async fn settle(
        &self,
        task: TaskId,
        outcome: WorkflowTaskOutcome,
    ) -> Result<(), WorkflowRunError>;
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WorkflowRunError {
    #[error("workflow persistence failed: {0}")]
    Persistence(String),
    #[error("workflow run contains an unsettled task `{step}`; inspect its child before retrying")]
    UnsettledTask { step: String },
    #[error("workflow definition differs from the durable run")]
    DefinitionChanged,
    #[error("workflow graph became unrunnable")]
    InvalidGraph,
    #[error("workflow step `{step}` failed: {message}")]
    StepFailed { step: String, message: String },
    #[error("workflow artifact limit exceeded by step `{step}`")]
    ArtifactLimit { step: String },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::{
        WorkflowRunner, WorkflowStepArtifact, WorkflowStepExecutionError, WorkflowStepExecutor,
        WorkflowStepRequest,
    };
    use crate::{ExtensionCatalog, ExtensionDiscoveryConfig};

    fn write_workflow(root: &std::path::Path, body: &str) {
        let path = root.join(".agents/workflows/delivery.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, body).expect("workflow");
    }

    fn artifact(text: impl Into<String>) -> WorkflowStepArtifact {
        WorkflowStepArtifact {
            subagent_id: rw_types::SubagentId("agent".to_owned()),
            child_session_id: rw_types::SessionId("child".to_owned()),
            final_text: text.into(),
            touched_files: Vec::new(),
            diff_artifact: None,
            usage: rw_types::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: rw_types::Cost::Unavailable {
                reason: "fixture".to_owned(),
            },
        }
    }

    struct MemoryJournal(Mutex<rw_types::workflow::WorkflowRunState>);
    impl MemoryJournal {
        fn new(workflow: &super::DiscoveredWorkflow) -> Self {
            Self(Mutex::new(rw_types::workflow::WorkflowRunState {
                run_id: rw_types::workflow::WorkflowRunId::parse("0".repeat(32)).expect("run"),
                parent_session_id: rw_types::SessionId("parent".to_owned()),
                workflow: workflow.name().to_owned(),
                definition_digest: workflow.definition_digest().expect("digest"),
                tasks: workflow
                    .steps()
                    .iter()
                    .map(|step| {
                        (
                            step.id().to_owned(),
                            rw_types::workflow::WorkflowTaskState::Pending,
                        )
                    })
                    .collect(),
            }))
        }
    }
    #[async_trait]
    impl super::WorkflowJournal for MemoryJournal {
        async fn state(
            &self,
        ) -> Result<rw_types::workflow::WorkflowRunState, super::WorkflowRunError> {
            Ok(self.0.lock().expect("state").clone())
        }
        async fn claim(
            &self,
            tasks: Vec<rw_types::workflow::TaskId>,
        ) -> Result<(), super::WorkflowRunError> {
            for task in tasks {
                self.0.lock().expect("state").tasks.insert(
                    task.step_id,
                    rw_types::workflow::WorkflowTaskState::Started { child: None },
                );
            }
            Ok(())
        }
        async fn bind_child(
            &self,
            task: rw_types::workflow::TaskId,
            child: rw_types::workflow::WorkflowChild,
        ) -> Result<(), super::WorkflowRunError> {
            self.0.lock().expect("state").tasks.insert(
                task.step_id,
                rw_types::workflow::WorkflowTaskState::Started { child: Some(child) },
            );
            Ok(())
        }
        async fn settle(
            &self,
            task: rw_types::workflow::TaskId,
            outcome: rw_types::workflow::WorkflowTaskOutcome,
        ) -> Result<(), super::WorkflowRunError> {
            self.0.lock().expect("state").tasks.insert(
                task.step_id,
                rw_types::workflow::WorkflowTaskState::Settled { outcome },
            );
            Ok(())
        }
    }

    struct ReplayExecutor {
        events: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl WorkflowStepExecutor for ReplayExecutor {
        async fn execute_step(
            &self,
            request: WorkflowStepRequest,
        ) -> Result<WorkflowStepArtifact, WorkflowStepExecutionError> {
            self.events
                .lock()
                .expect("events")
                .push(format!("start:{}", request.step_id));
            if matches!(request.step_id.as_str(), "impl" | "tests") {
                tokio::task::yield_now().await;
            }
            let artifact_names = request
                .artifacts
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join("+");
            self.events
                .lock()
                .expect("events")
                .push(format!("finish:{}", request.step_id));
            Ok(artifact(format!("{}[{artifact_names}]", request.step_id)))
        }
    }

    #[tokio::test]
    async fn replay_plan_parallel_implementation_tests_then_review_is_exact() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write_workflow(
            &project,
            r#"name = "delivery"
description = "Replay acceptance workflow"

[[step]]
id = "plan"
agent = "plan"

[[step]]
id = "impl"
agent = "general"
needs = ["plan"]
parallel = true

[[step]]
id = "tests"
command = "test"
needs = ["plan"]
parallel = true

[[step]]
id = "review"
agent = "explore"
needs = ["impl", "tests"]
"#,
        );
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
        );
        let workflow = catalog.workflow("delivery").expect("workflow");
        let events = Arc::new(Mutex::new(Vec::new()));
        let executor = ReplayExecutor {
            events: Arc::clone(&events),
        };

        let journal = MemoryJournal::new(workflow);
        let report = WorkflowRunner::new(&executor, &journal)
            .run(workflow)
            .await
            .expect("workflow run");

        assert_eq!(
            report
                .steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            vec!["plan", "impl", "tests", "review"]
        );
        assert_eq!(
            report.steps[3]
                .output
                .as_ref()
                .map(|output| output.final_text.as_str()),
            Some("review[impl+tests]")
        );
        assert_eq!(
            *events.lock().expect("events"),
            vec![
                "start:plan",
                "finish:plan",
                "start:impl",
                "start:tests",
                "finish:impl",
                "finish:tests",
                "start:review",
                "finish:review",
            ]
        );
    }

    #[test]
    fn malformed_cycles_and_resource_limits_fail_discovery() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write_workflow(
            &project,
            r#"description = "cycle"
[[step]]
id = "a"
agent = "plan"
needs = ["b"]
[[step]]
id = "b"
agent = "plan"
needs = ["a"]
"#,
        );
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
        );
        assert!(catalog.workflow("delivery").is_none());
        assert!(catalog.diagnostics()[0].message().contains("cycle"));
    }

    struct OversizedExecutor;

    #[async_trait]
    impl WorkflowStepExecutor for OversizedExecutor {
        async fn execute_step(
            &self,
            _request: WorkflowStepRequest,
        ) -> Result<WorkflowStepArtifact, WorkflowStepExecutionError> {
            Ok(artifact(
                "x".repeat(rw_types::workflow::MAX_STEP_ARTIFACT_BYTES + 1),
            ))
        }
    }

    #[tokio::test]
    async fn oversized_step_artifact_fails_closed() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write_workflow(
            &project,
            "description = \"bounded\"\n[[step]]\nid = \"plan\"\nagent = \"plan\"\n",
        );
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
        );

        let journal = MemoryJournal::new(catalog.workflow("delivery").expect("workflow"));
        let error = WorkflowRunner::new(&OversizedExecutor, &journal)
            .run(catalog.workflow("delivery").expect("workflow"))
            .await
            .expect_err("artifact rejected");

        assert_eq!(
            error,
            super::WorkflowRunError::ArtifactLimit {
                step: "plan".to_owned()
            }
        );
    }

    struct ConditionalExecutor;

    #[async_trait]
    impl WorkflowStepExecutor for ConditionalExecutor {
        async fn execute_step(
            &self,
            request: WorkflowStepRequest,
        ) -> Result<WorkflowStepArtifact, WorkflowStepExecutionError> {
            if request.step_id == "test" {
                Err(WorkflowStepExecutionError::failed(
                    "expected replay failure",
                ))
            } else {
                Ok(artifact(request.step_id))
            }
        }
    }

    #[tokio::test]
    async fn success_and_failure_conditions_are_replay_deterministic() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write_workflow(
            &project,
            r#"description = "conditions"
[[step]]
id = "test"
command = "test"
on-fail = "continue"

[[step]]
id = "fix"
agent = "general"
needs = ["test"]
if = "failure:test"

[[step]]
id = "publish"
command = "publish"
needs = ["test"]
if = "success:test"
"#,
        );
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
        );

        let journal = MemoryJournal::new(catalog.workflow("delivery").expect("workflow"));
        let report = WorkflowRunner::new(&ConditionalExecutor, &journal)
            .run(catalog.workflow("delivery").expect("workflow"))
            .await
            .expect("continued workflow");

        assert!(report.steps[0].error.is_some());
        assert_eq!(
            report.steps[1]
                .output
                .as_ref()
                .map(|output| output.final_text.as_str()),
            Some("fix")
        );
        assert!(report.steps[2].skipped);
    }
    #[tokio::test]
    async fn resumed_run_reuses_completed_dependencies_and_rejects_ambiguous_work() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        write_workflow(
            &project,
            "description = \"resume\"\n[[step]]\nid = \"plan\"\nagent = \"plan\"\n[[step]]\nid = \"build\"\nagent = \"general\"\nneeds = [\"plan\"]\n",
        );
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, fixture.path().join("home"))
                .with_project_trusted(true),
        );
        let workflow = catalog.workflow("delivery").expect("workflow");
        let journal = MemoryJournal::new(workflow);
        journal.0.lock().expect("state").tasks.insert(
            "plan".to_owned(),
            rw_types::workflow::WorkflowTaskState::Settled {
                outcome: rw_types::workflow::WorkflowTaskOutcome::Completed {
                    artifact: Arc::new(artifact("saved plan")),
                },
            },
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let executor = ReplayExecutor {
            events: Arc::clone(&events),
        };
        let report = WorkflowRunner::new(&executor, &journal)
            .run(workflow)
            .await
            .expect("resume");
        assert_eq!(
            report.steps[0].output.as_ref().expect("plan").final_text,
            "saved plan"
        );
        assert_eq!(
            *events.lock().expect("events"),
            ["start:build", "finish:build"]
        );
        events.lock().expect("events").clear();
        WorkflowRunner::new(&executor, &journal)
            .run(workflow)
            .await
            .expect("terminal replay");
        assert!(events.lock().expect("events").is_empty());
        journal.0.lock().expect("state").tasks.insert(
            "build".to_owned(),
            rw_types::workflow::WorkflowTaskState::Started { child: None },
        );
        assert!(matches!(
            WorkflowRunner::new(&executor, &journal).run(workflow).await,
            Err(super::WorkflowRunError::UnsettledTask { .. })
        ));
        assert!(events.lock().expect("events").is_empty());
    }

    struct UnsettledExecutor;
    #[async_trait]
    impl WorkflowStepExecutor for UnsettledExecutor {
        async fn execute_step(
            &self,
            request: WorkflowStepRequest,
        ) -> Result<WorkflowStepArtifact, WorkflowStepExecutionError> {
            if request.step_id == "uncertain" {
                Err(WorkflowStepExecutionError::unsettled("cleanup missing"))
            } else if request.step_id == "oversized" {
                Ok(artifact(
                    "x".repeat(rw_types::workflow::MAX_STEP_ARTIFACT_BYTES + 1),
                ))
            } else {
                Ok(artifact(request.step_id))
            }
        }
    }

    #[tokio::test]
    async fn uncertain_peer_does_not_discard_a_settled_parallel_receipt() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        write_workflow(
            &project,
            "description = \"settlement\"\n[[step]]\nid = \"uncertain\"\nagent = \"plan\"\nparallel = true\non-fail = \"continue\"\n[[step]]\nid = \"done\"\nagent = \"general\"\nparallel = true\n",
        );
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, fixture.path().join("home"))
                .with_project_trusted(true),
        );
        let workflow = catalog.workflow("delivery").expect("workflow");
        let journal = MemoryJournal::new(workflow);
        assert!(matches!(
            WorkflowRunner::new(&UnsettledExecutor, &journal)
                .run(workflow)
                .await,
            Err(super::WorkflowRunError::UnsettledTask { .. })
        ));
        let state = journal.0.lock().expect("state");
        assert!(matches!(
            state.tasks["uncertain"],
            rw_types::workflow::WorkflowTaskState::Started { .. }
        ));
        assert!(matches!(
            state.tasks["done"],
            rw_types::workflow::WorkflowTaskState::Settled { .. }
        ));
    }
    #[tokio::test]
    async fn oversized_peer_does_not_discard_a_settled_parallel_receipt() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        write_workflow(
            &project,
            r#"description = "limits"
[[step]]
id = "oversized"
agent = "plan"
parallel = true
[[step]]
id = "done"
agent = "general"
parallel = true
"#,
        );
        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(project, fixture.path().join("home"))
                .with_project_trusted(true),
        );
        let workflow = catalog.workflow("delivery").expect("workflow");
        let journal = MemoryJournal::new(workflow);
        assert!(matches!(
            WorkflowRunner::new(&UnsettledExecutor, &journal)
                .run(workflow)
                .await,
            Err(super::WorkflowRunError::ArtifactLimit { .. })
        ));
        let state = journal.0.lock().expect("state");
        assert!(matches!(
            state.tasks["oversized"],
            rw_types::workflow::WorkflowTaskState::Started { .. }
        ));
        assert!(matches!(
            state.tasks["done"],
            rw_types::workflow::WorkflowTaskState::Settled { .. }
        ));
    }
}
