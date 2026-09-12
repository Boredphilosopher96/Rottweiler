use super::{
    DiscoveredWorkflow, WorkflowCondition, WorkflowJournal, WorkflowOnFail, WorkflowRunError,
    WorkflowRunReport, WorkflowStep, WorkflowStepArtifact, WorkflowStepExecutionError,
    WorkflowStepExecutor, WorkflowStepReport, WorkflowStepRequest,
};
use futures_util::future::join_all;
use rw_types::workflow::{
    MAX_WORKFLOW_ARTIFACT_BYTES, TaskId, WorkflowRunId, WorkflowRunState, WorkflowTaskOutcome,
    WorkflowTaskState, workflow_outcome_bytes,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Resumes settled scheduler state without repeating any previously started effect.
pub struct WorkflowRunner<'a, Executor> {
    executor: &'a Executor,
    journal: &'a dyn WorkflowJournal,
}

struct Scheduler {
    run_id: WorkflowRunId,
    complete: BTreeSet<String>,
    reports: BTreeMap<String, WorkflowStepReport>,
    artifacts: BTreeMap<String, Arc<WorkflowStepArtifact>>,
    artifact_bytes: usize,
    next_execution_index: usize,
}

impl<'a, Executor: WorkflowStepExecutor> WorkflowRunner<'a, Executor> {
    #[must_use]
    pub fn new(executor: &'a Executor, journal: &'a dyn WorkflowJournal) -> Self {
        Self { executor, journal }
    }

    /// Run pending steps from a matching durable scheduler snapshot.
    ///
    /// # Errors
    /// Rejects changed definitions, unsettled tasks, failed stop nodes or persistence failure.
    pub async fn run(
        &self,
        workflow: &DiscoveredWorkflow,
    ) -> Result<WorkflowRunReport, WorkflowRunError> {
        let mut state = Scheduler::restore(workflow, self.journal.state().await?)?;
        check_stop(workflow, &state.reports)?;
        while state.complete.len() != workflow.steps.len() {
            let (skipped, wave): (Vec<_>, Vec<_>) = state
                .ready_wave(workflow)?
                .into_iter()
                .partition(|step| !condition_matches(&step.condition, &state.reports));
            for step in skipped {
                self.journal
                    .settle(state.task(&step.id), WorkflowTaskOutcome::Skipped)
                    .await?;
                state.record(&step.id, WorkflowTaskOutcome::Skipped);
            }
            if wave.is_empty() {
                continue;
            }
            self.journal
                .claim(wave.iter().map(|step| state.task(&step.id)).collect())
                .await?;
            let requests = state.requests(workflow, &wave);
            let results = join_all(
                requests
                    .into_iter()
                    .map(|request| self.executor.execute_step(request)),
            )
            .await;
            self.persist_wave(&mut state, wave, results).await?;
            check_stop(workflow, &state.reports)?;
        }
        Ok(WorkflowRunReport {
            workflow: workflow.name.clone(),
            steps: workflow
                .steps
                .iter()
                .filter_map(|step| state.reports.remove(&step.id))
                .collect(),
        })
    }

    async fn persist_wave(
        &self,
        state: &mut Scheduler,
        wave: Vec<&WorkflowStep>,
        results: Vec<Result<WorkflowStepArtifact, WorkflowStepExecutionError>>,
    ) -> Result<(), WorkflowRunError> {
        let mut failure = None;
        for (step, result) in wave.into_iter().zip(results) {
            let outcome = match result {
                Ok(artifact) => WorkflowTaskOutcome::Completed {
                    artifact: Arc::new(artifact),
                },
                Err(WorkflowStepExecutionError::Failed { message }) => {
                    WorkflowTaskOutcome::Failed { message }
                }
                Err(WorkflowStepExecutionError::Unsettled { .. }) => {
                    failure.get_or_insert_with(|| WorkflowRunError::UnsettledTask {
                        step: step.id.clone(),
                    });
                    continue;
                }
            };
            let bytes = match state.outcome_bytes(&step.id, &outcome) {
                Ok(bytes) => bytes,
                Err(error) => {
                    failure.get_or_insert(error);
                    continue;
                }
            };
            match self
                .journal
                .settle(state.task(&step.id), outcome.clone())
                .await
            {
                Ok(()) => {
                    state.artifact_bytes += bytes;
                    state.record(&step.id, outcome);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Scheduler {
    fn restore(
        workflow: &DiscoveredWorkflow,
        saved: WorkflowRunState,
    ) -> Result<Self, WorkflowRunError> {
        if saved.workflow != workflow.name
            || saved.definition_digest != workflow.definition_digest()?
            || !saved.tasks.keys().eq(workflow
                .steps
                .iter()
                .map(|step| &step.id)
                .collect::<BTreeSet<_>>())
        {
            return Err(WorkflowRunError::DefinitionChanged);
        }
        let mut state = Self {
            run_id: saved.run_id,
            complete: BTreeSet::new(),
            reports: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            artifact_bytes: 0,
            next_execution_index: 0,
        };
        for (id, task) in saved.tasks {
            match task {
                WorkflowTaskState::Started { .. } => {
                    return Err(WorkflowRunError::UnsettledTask { step: id });
                }
                WorkflowTaskState::Settled { outcome } => {
                    state.artifact_bytes += state.outcome_bytes(&id, &outcome)?;
                    state.record(&id, outcome);
                }
                WorkflowTaskState::Pending => {}
            }
        }
        Ok(state)
    }

    fn outcome_bytes(
        &self,
        id: &str,
        outcome: &WorkflowTaskOutcome,
    ) -> Result<usize, WorkflowRunError> {
        let failure = || WorkflowRunError::ArtifactLimit {
            step: id.to_owned(),
        };
        let bytes = workflow_outcome_bytes(outcome).map_err(|_| failure())?;
        if bytes > MAX_WORKFLOW_ARTIFACT_BYTES.saturating_sub(self.artifact_bytes) {
            return Err(failure());
        }
        Ok(bytes)
    }

    fn task(&self, step_id: &str) -> TaskId {
        TaskId {
            run_id: self.run_id.clone(),
            step_id: step_id.to_owned(),
        }
    }

    fn ready_wave<'w>(
        &self,
        workflow: &'w DiscoveredWorkflow,
    ) -> Result<Vec<&'w WorkflowStep>, WorkflowRunError> {
        let ready: Vec<_> = workflow
            .steps
            .iter()
            .filter(|step| {
                !self.complete.contains(&step.id)
                    && step.needs.iter().all(|need| self.complete.contains(need))
            })
            .collect();
        let first = ready.first().ok_or(WorkflowRunError::InvalidGraph)?;
        if first.parallel {
            Ok(ready.into_iter().take_while(|step| step.parallel).collect())
        } else {
            Ok(vec![*first])
        }
    }

    fn requests(
        &mut self,
        workflow: &DiscoveredWorkflow,
        wave: &[&WorkflowStep],
    ) -> Vec<WorkflowStepRequest> {
        let requests: Vec<_> = wave
            .iter()
            .enumerate()
            .map(|(offset, step)| WorkflowStepRequest {
                task_id: self.task(&step.id),
                workflow: workflow.name.clone(),
                step_index: self.next_execution_index + offset,
                step_id: step.id.clone(),
                target: step.target.clone(),
                prompt: step.prompt.clone(),
                artifacts: step
                    .inputs
                    .iter()
                    .filter_map(|id| {
                        self.artifacts
                            .get(id)
                            .map(|value| (id.clone(), value.clone()))
                    })
                    .collect(),
            })
            .collect();
        self.next_execution_index += requests.len();
        requests
    }

    fn record(&mut self, id: &str, outcome: WorkflowTaskOutcome) {
        let report = match outcome {
            WorkflowTaskOutcome::Completed { artifact } => {
                self.artifacts.insert(id.to_owned(), Arc::clone(&artifact));
                WorkflowStepReport {
                    id: id.to_owned(),
                    output: Some(artifact),
                    error: None,
                    skipped: false,
                }
            }
            WorkflowTaskOutcome::Failed { message } => WorkflowStepReport {
                id: id.to_owned(),
                output: None,
                error: Some(message),
                skipped: false,
            },
            WorkflowTaskOutcome::Skipped => WorkflowStepReport {
                id: id.to_owned(),
                output: None,
                error: None,
                skipped: true,
            },
        };
        self.reports.insert(id.to_owned(), report);
        self.complete.insert(id.to_owned());
    }
}

fn check_stop(
    workflow: &DiscoveredWorkflow,
    reports: &BTreeMap<String, WorkflowStepReport>,
) -> Result<(), WorkflowRunError> {
    for step in &workflow.steps {
        if step.on_fail == WorkflowOnFail::Stop
            && let Some(message) = reports
                .get(&step.id)
                .and_then(|report| report.error.as_ref())
        {
            return Err(WorkflowRunError::StepFailed {
                step: step.id.clone(),
                message: message.clone(),
            });
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
