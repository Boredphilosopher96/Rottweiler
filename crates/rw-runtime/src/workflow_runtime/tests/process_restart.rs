//! A real task effect outlives its runner; restart must not repeat ambiguous work.
#![allow(clippy::expect_used)]
use super::DurableWorkflowJournal;
use async_trait::async_trait;
use rw_ext::{
    ExtensionCatalog, ExtensionDiscoveryConfig, WorkflowJournal as _, WorkflowRunError,
    WorkflowRunner, WorkflowStepArtifact, WorkflowStepExecutionError, WorkflowStepExecutor,
    WorkflowStepRequest,
};
use rw_resources::process::BlockingProcess;
use rw_types::{
    Cost, SessionId, SubagentId, Usage,
    workflow::{WorkflowChild, WorkflowRunId, WorkflowTaskOutcome, WorkflowTaskState},
};
use std::{
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const CHILD_ROOT: &str = "RW_DURABLE_RUNNER_PROCESS_ROOT";
const CHILD_PHASE: &str = "RW_DURABLE_RUNNER_PROCESS_PHASE";
const TEST: &str = "workflow_runtime::process_restart_tests::durable_runner_restart_preserves_effect_and_refuses_reexecution";
type TestResult = Result<(), Box<dyn std::error::Error>>;

struct EffectTask {
    root: PathBuf,
    restarting: bool,
    journal: Arc<DurableWorkflowJournal>,
}

#[async_trait]
impl WorkflowStepExecutor for EffectTask {
    async fn execute_step(
        &self,
        request: WorkflowStepRequest,
    ) -> Result<WorkflowStepArtifact, WorkflowStepExecutionError> {
        assert!(!self.restarting, "restart must not dispatch any task");
        let child = WorkflowChild {
            subagent_id: SubagentId(format!("completed-{}", request.step_id)),
            session_id: SessionId(format!("{}-session", request.step_id)),
        };
        self.journal
            .bind_child(request.task_id, child.clone())
            .await
            .map_err(|error| WorkflowStepExecutionError::unsettled(error.to_string()))?;
        let root = self.root.clone();
        let step = request.step_id.clone();
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let mut effects = OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("effects"))?;
            writeln!(effects, "{step}")?;
            effects.sync_all()?;
            if step == "build" {
                // Publish only after the physical task effect is durable.
                let ready = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(root.join("effect-ready"))?;
                ready.sync_all()?;
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|error| WorkflowStepExecutionError::unsettled(error.to_string()))?
        .map_err(|error| WorkflowStepExecutionError::unsettled(error.to_string()))?;
        if request.step_id == "build" {
            // Keep the runner between effect completion and journal settlement.
            return std::future::pending().await;
        }
        Ok(WorkflowStepArtifact {
            subagent_id: child.subagent_id,
            child_session_id: child.session_id,
            final_text: "durable plan artifact".into(),
            touched_files: Vec::new(),
            diff_artifact: None,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Unavailable {
                reason: "fixture".into(),
            },
        })
    }
}

async fn child_run(root: &Path, restarting: bool) -> TestResult {
    let catalog = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(root.join("project"), root.join("home"))
            .with_project_trusted(true),
    );
    let workflow = catalog.workflow("delivery").ok_or("workflow missing")?;
    let journal = DurableWorkflowJournal::open(
        root.join("journal"),
        WorkflowRunId::parse("0123456789abcdef0123456789abcdef".into())?,
        SessionId("parent".into()),
        workflow,
    )
    .await?;
    let executor = EffectTask {
        root: root.to_owned(),
        restarting,
        journal: Arc::clone(&journal),
    };
    let result = WorkflowRunner::new(&executor, journal.as_ref())
        .run(workflow)
        .await;
    assert!(
        restarting,
        "initial runner returned before effect readiness: {result:?}"
    );
    assert!(matches!(result, Err(WorkflowRunError::UnsettledTask { step }) if step == "build"));
    let state = journal.state().await?;
    let WorkflowTaskState::Settled {
        outcome: WorkflowTaskOutcome::Completed { artifact },
    } = &state.tasks["plan"]
    else {
        panic!("completed dependency was lost")
    };
    assert_eq!(artifact.final_text, "durable plan artifact");
    assert!(matches!(
        &state.tasks["build"],
        WorkflowTaskState::Started { child: Some(child) }
            if child.subagent_id.0 == "completed-build" && child.session_id.0 == "build-session"
    ));
    assert!(matches!(state.tasks["review"], WorkflowTaskState::Pending));
    assert_eq!(fs::read_to_string(root.join("effects"))?, "plan\nbuild\n");
    Ok(())
}

fn spawn(root: &Path, phase: &str) -> Result<BlockingProcess, Box<dyn std::error::Error>> {
    let diagnostics = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(root.join(format!("{phase}.log")))?;
    Ok(BlockingProcess::spawn(
        Command::new(std::env::current_exe()?)
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD_ROOT, root)
            .env(CHILD_PHASE, phase)
            .stdin(Stdio::null())
            .stdout(diagnostics.try_clone()?)
            .stderr(diagnostics),
    )?)
}

fn failure_diagnostic(path: &Path) -> String {
    const LIMIT: usize = 64 * 1024;
    let read = (|| {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    })();
    match read {
        Ok(mut bytes) => {
            let truncated = bytes.len() > LIMIT;
            bytes.truncate(LIMIT);
            format!(
                "{}{}",
                String::from_utf8_lossy(&bytes),
                if truncated { " [truncated]" } else { "" }
            )
        }
        Err(error) => format!("[diagnostic unavailable: {error}]"),
    }
}

fn await_ready(
    child: &BlockingProcess,
    diagnostics: &Path,
    ready: impl Fn() -> bool,
) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_status()? {
            return Err(format!(
                "task runner exited before readiness ({status}): {}",
                failure_diagnostic(diagnostics)
            )
            .into());
        }
        // A published marker cannot turn an already failed child into a live-runner proof.
        if ready() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "task runner readiness timed out: {}",
                failure_diagnostic(diagnostics)
            )
            .into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn completed_child_cannot_pass_runner_readiness_from_an_existing_marker() -> TestResult {
    let root = tempfile::tempdir()?;
    let diagnostics = root.path().join("initial.log");
    fs::write(
        &diagnostics,
        format!("first runner failure\n{}", "x".repeat(128 * 1024)),
    )?;
    let mut child = BlockingProcess::spawn(Command::new("/bin/sh").args(["-c", "exit 7"]))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_status()?.is_none() {
        assert!(Instant::now() < deadline, "exit fixture timed out");
        thread::sleep(Duration::from_millis(10));
    }
    let result = await_ready(&child, &diagnostics, || true);
    child.settle();
    let error = result
        .expect_err("existing marker cannot hide an exited child")
        .to_string();
    assert!(error.contains("first runner failure"));
    assert!(error.contains("[truncated]"));
    assert!(error.len() < 64 * 1024 + 256);
    fs::remove_file(&diagnostics)?;
    assert!(failure_diagnostic(&diagnostics).starts_with("[diagnostic unavailable:"));
    Ok(())
}

#[test]
fn durable_runner_restart_preserves_effect_and_refuses_reexecution() -> TestResult {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let restarting = std::env::var(CHILD_PHASE)? == "restart";
        return tokio::runtime::Runtime::new()?.block_on(child_run(Path::new(&root), restarting));
    }
    let root = tempfile::Builder::new()
        .prefix("rw-a31-restart-")
        .tempdir()?;
    // Unwind the process owners before deciding whether to retain their evidence.
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parent_run(root.path())));
    if !matches!(&outcome, Ok(Ok(()))) {
        let retained = root.keep();
        eprintln!(
            "workflow restart failure evidence retained at {}",
            retained.display()
        );
    }
    match outcome {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn parent_run(root: &Path) -> TestResult {
    let workflow = root.join("project/.agents/workflows/delivery.toml");
    fs::create_dir_all(workflow.parent().ok_or("workflow parent")?)?;
    fs::write(
        workflow,
        "description = \"restart\"\n[[step]]\nid = \"plan\"\nagent = \"plan\"\n[[step]]\nid = \"build\"\nagent = \"general\"\nneeds = [\"plan\"]\n[[step]]\nid = \"review\"\nagent = \"explore\"\nneeds = [\"build\"]\n",
    )?;
    let mut first = spawn(root, "initial")?;
    await_ready(&first, &root.join("initial.log"), || {
        root.join("effect-ready").exists()
    })?;
    assert_eq!(fs::read_to_string(root.join("effects"))?, "plan\nbuild\n");
    first.settle(); // SIGKILL and joined group retirement, without runner destructors.
    let mut restarted = spawn(root, "restart")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = restarted.try_status()? {
            break status;
        }
        assert!(Instant::now() < deadline, "restarted runner timed out");
        thread::sleep(Duration::from_millis(10));
    };
    restarted.settle();
    assert!(
        status.success(),
        "restart failed: {}",
        failure_diagnostic(&root.join("restart.log"))
    );
    assert_eq!(fs::read_to_string(root.join("effects"))?, "plan\nbuild\n");
    Ok(())
}
