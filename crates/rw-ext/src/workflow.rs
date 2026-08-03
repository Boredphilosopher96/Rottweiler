use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use futures_util::future::join_all;
use rw_types::{Cost, DiffArtifactRef, SessionId, SubagentId, Usage};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::discovery::{
    ArtifactLocation, ArtifactOrigin, ArtifactScope, ExtensionDiscoveryError,
    read_bounded_relative_utf8,
};

const MAX_WORKFLOW_BYTES: u64 = 1024 * 1024;
const MAX_WORKFLOW_STEPS: usize = 64;
const MAX_WORKFLOW_EDGES: usize = 256;
const MAX_STEP_ARTIFACT_BYTES: usize = 256 * 1024;
const MAX_WORKFLOW_ARTIFACT_BYTES: usize = 1024 * 1024;

/// Executable reference selected by one workflow step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowStepTarget {
    Agent(String),
    Command(String),
}

/// Failure policy for a workflow step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowOnFail {
    Stop,
    Continue,
}

/// Minimal deterministic condition evaluated from a completed dependency.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowCondition {
    Always,
    Success(String),
    Failure(String),
}

/// One validated node in a declarative workflow DAG.
#[derive(Clone, Debug, Eq, PartialEq)]
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

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
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
    pub workflow: String,
    /// Stable contiguous position among nodes that actually execute.
    pub step_index: usize,
    pub step_id: String,
    pub target: WorkflowStepTarget,
    pub prompt: String,
    pub artifacts: BTreeMap<String, WorkflowStepArtifact>,
}

/// Bounded typed output retained for downstream workflow nodes and reports.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowStepArtifact {
    pub subagent_id: SubagentId,
    pub child_session_id: SessionId,
    pub final_text: String,
    pub touched_files: Vec<String>,
    pub diff_artifact: Option<DiffArtifactRef>,
    pub usage: Usage,
    pub cost: Cost,
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
#[error("{message}")]
pub struct WorkflowStepExecutionError {
    message: String,
}

impl WorkflowStepExecutionError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Stable status for one completed workflow node.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowStepReport {
    pub id: String,
    pub output: Option<WorkflowStepArtifact>,
    pub error: Option<String>,
    pub skipped: bool,
}

/// Successful workflow result in definition order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowRunReport {
    pub workflow: String,
    pub steps: Vec<WorkflowStepReport>,
}

/// Deterministic, resource-bounded declarative workflow runner.
pub struct WorkflowRunner<'a, Executor> {
    executor: &'a Executor,
}

impl<'a, Executor> WorkflowRunner<'a, Executor>
where
    Executor: WorkflowStepExecutor,
{
    #[must_use]
    pub const fn new(executor: &'a Executor) -> Self {
        Self { executor }
    }

    /// Runs ready sequential nodes one at a time and ready `parallel = true`
    /// nodes as a stable concurrent wave.
    ///
    /// # Errors
    ///
    /// Stops on a failed `on-fail = "stop"` node or when artifact limits are
    /// exceeded.
    pub async fn run(
        &self,
        workflow: &DiscoveredWorkflow,
    ) -> Result<WorkflowRunReport, WorkflowRunError> {
        let mut complete = BTreeSet::new();
        let mut artifacts = BTreeMap::<String, WorkflowStepArtifact>::new();
        let mut reports = BTreeMap::<String, WorkflowStepReport>::new();
        let mut total_artifact_bytes = 0_usize;
        let mut next_execution_index = 0_usize;
        while complete.len() != workflow.steps.len() {
            let ready = workflow
                .steps
                .iter()
                .filter(|step| {
                    !complete.contains(&step.id)
                        && step.needs.iter().all(|need| complete.contains(need))
                })
                .collect::<Vec<_>>();
            let Some(first) = ready.first() else {
                return Err(WorkflowRunError::InvalidGraph);
            };
            let wave = if first.parallel {
                ready
                    .into_iter()
                    .take_while(|step| step.parallel)
                    .collect::<Vec<_>>()
            } else {
                vec![*first]
            };
            let (skipped, wave): (Vec<_>, Vec<_>) = wave
                .into_iter()
                .partition(|step| !condition_matches(&step.condition, &reports));
            for step in skipped {
                reports.insert(
                    step.id.clone(),
                    WorkflowStepReport {
                        id: step.id.clone(),
                        output: None,
                        error: None,
                        skipped: true,
                    },
                );
                complete.insert(step.id.clone());
            }
            if wave.is_empty() {
                continue;
            }
            let wave_start = next_execution_index;
            let requests = wave
                .iter()
                .enumerate()
                .map(|(offset, step)| WorkflowStepRequest {
                    workflow: workflow.name.clone(),
                    step_index: wave_start.saturating_add(offset),
                    step_id: step.id.clone(),
                    target: step.target.clone(),
                    prompt: step.prompt.clone(),
                    artifacts: step
                        .inputs
                        .iter()
                        .filter_map(|id| artifacts.get(id).map(|value| (id.clone(), value.clone())))
                        .collect(),
                })
                .collect::<Vec<_>>();
            next_execution_index = next_execution_index.saturating_add(requests.len());
            let results = join_all(
                requests
                    .into_iter()
                    .map(|request| self.executor.execute_step(request)),
            )
            .await;
            for (step, result) in wave.into_iter().zip(results) {
                record_step_result(
                    step,
                    result,
                    &mut total_artifact_bytes,
                    &mut artifacts,
                    &mut reports,
                )?;
                complete.insert(step.id.clone());
            }
        }
        Ok(WorkflowRunReport {
            workflow: workflow.name.clone(),
            steps: workflow
                .steps
                .iter()
                .filter_map(|step| reports.remove(&step.id))
                .collect(),
        })
    }
}

fn record_step_result(
    step: &WorkflowStep,
    result: Result<WorkflowStepArtifact, WorkflowStepExecutionError>,
    total_artifact_bytes: &mut usize,
    artifacts: &mut BTreeMap<String, WorkflowStepArtifact>,
    reports: &mut BTreeMap<String, WorkflowStepReport>,
) -> Result<(), WorkflowRunError> {
    match result {
        Ok(output) => {
            let output_bytes = serde_json::to_vec(&output)
                .map_err(|_| WorkflowRunError::ArtifactLimit {
                    step: step.id.clone(),
                })?
                .len();
            if output_bytes > MAX_STEP_ARTIFACT_BYTES
                || total_artifact_bytes.saturating_add(output_bytes) > MAX_WORKFLOW_ARTIFACT_BYTES
            {
                return Err(WorkflowRunError::ArtifactLimit {
                    step: step.id.clone(),
                });
            }
            *total_artifact_bytes = total_artifact_bytes.saturating_add(output_bytes);
            artifacts.insert(step.id.clone(), output.clone());
            reports.insert(
                step.id.clone(),
                WorkflowStepReport {
                    id: step.id.clone(),
                    output: Some(output),
                    error: None,
                    skipped: false,
                },
            );
        }
        Err(error) => {
            if step.on_fail == WorkflowOnFail::Stop {
                return Err(WorkflowRunError::StepFailed {
                    step: step.id.clone(),
                    message: error.to_string(),
                });
            }
            reports.insert(
                step.id.clone(),
                WorkflowStepReport {
                    id: step.id.clone(),
                    output: None,
                    error: Some(error.to_string()),
                    skipped: false,
                },
            );
        }
    }
    Ok(())
}

fn condition_matches(
    condition: &WorkflowCondition,
    reports: &BTreeMap<String, WorkflowStepReport>,
) -> bool {
    match condition {
        WorkflowCondition::Always => true,
        WorkflowCondition::Success(step) => reports
            .get(step)
            .is_some_and(|report| report.output.is_some() && !report.skipped),
        WorkflowCondition::Failure(step) => reports
            .get(step)
            .is_some_and(|report| report.error.is_some() && !report.skipped),
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WorkflowRunError {
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

        let report = WorkflowRunner::new(&executor)
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
            Ok(artifact("x".repeat(super::MAX_STEP_ARTIFACT_BYTES + 1)))
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

        let error = WorkflowRunner::new(&OversizedExecutor)
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
                Err(WorkflowStepExecutionError::new("expected replay failure"))
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

        let report = WorkflowRunner::new(&ConditionalExecutor)
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
}
