mod presentation;
use presentation::{BACKGROUND_START_PRESENTATION, BASH_PRESENTATION};

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rw_sandbox::normalize_egress_domain;
use rw_types::ToolCapability;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::BackgroundProcessManager;
use crate::registry::{
    CancellationToken, CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError,
    ToolLimits, ToolOutputSink, ToolResult, input_schema, parse_input,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashInput {
    pub command: String,
    #[serde(default = "default_cwd")]
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Public domains requested in addition to the default package registries.
    /// Permission approvals bind to this exact normalized list.
    #[serde(default)]
    pub network_domains: Vec<String>,
    /// Selects the native OS sandbox boundary. Unsandboxed execution is an
    /// explicit escape hatch and is always permission-gated by the engine.
    #[serde(default)]
    pub sandbox: BashSandboxMode,
    /// Return immediately while the session process manager supervises the
    /// command. Output is retrieved with `background_output`.
    #[serde(default)]
    pub run_in_background: bool,
}

fn default_cwd() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest {
    pub command: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub network_domains: Vec<String>,
    pub sandbox: BashSandboxMode,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BashSandboxMode {
    #[default]
    Sandboxed,
    Unsandboxed,
    /// Internal write-denied sandbox used only after `run_in_background` has
    /// passed validation.
    ReadOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandOutcome {
    pub exit_code: i32,
}

/// Injected process boundary. Core must approve the bash manifest before this is called.
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    /// Waits for native cleanup transferred out of a dropped or panicked invocation.
    /// Implementations that own native effects must retain cleanup independently
    /// of `run`; pure replay implementations explicitly have no effects to settle.
    async fn settle_effects(&self) -> Result<(), ToolError>;

    /// Whether this executor can safely supervise a command after the
    /// initiating tool call returns.
    fn supports_background(&self) -> bool {
        false
    }

    /// Returns only after the command's effects have settled. The caller must
    /// retain this future through cancellation; `BashTool` owns foreground calls.
    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError>;
}

#[derive(Default)]
struct ForegroundCommands {
    calls: Mutex<Vec<Arc<ForegroundCommand>>>,
}

struct ForegroundCommand {
    _executor: Arc<dyn CommandExecutor>,
    cancellation: CancellationToken,
    abandoned: std::sync::atomic::AtomicBool,
    completion: tokio::sync::watch::Receiver<bool>,
}

struct ForegroundCaller {
    command: Arc<ForegroundCommand>,
    armed: bool,
}

impl Drop for ForegroundCaller {
    fn drop(&mut self) {
        if self.armed {
            self.command
                .abandoned
                .store(true, std::sync::atomic::Ordering::Release);
            self.command.cancellation.cancel();
        }
    }
}

impl ForegroundCommands {
    async fn settle_abandoned(&self) -> Result<(), ToolError> {
        loop {
            let abandoned = {
                let calls = self
                    .calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                calls
                    .iter()
                    .filter(|call| call.abandoned.load(std::sync::atomic::Ordering::Acquire))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if abandoned.is_empty() {
                return Ok(());
            }
            for command in abandoned {
                let mut completion = command.completion.clone();
                while !*completion.borrow_and_update() {
                    if completion.changed().await.is_err() {
                        return Err(ToolError::EffectsUnsettled(
                            "foreground command cleanup owner exited without settlement proof"
                                .to_owned(),
                        ));
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct BashTool {
    executor: Arc<dyn CommandExecutor>,
    foreground: Arc<ForegroundCommands>,
    limits: ToolLimits,
    safety: Arc<CommandSafetyClassifier>,
    background: Option<Arc<BackgroundProcessManager>>,
}

impl BashTool {
    #[must_use]
    pub fn new(executor: Arc<dyn CommandExecutor>, limits: ToolLimits) -> Self {
        Self {
            executor,
            foreground: Arc::new(ForegroundCommands::default()),
            limits,
            safety: Arc::new(CommandSafetyClassifier::default()),
            background: None,
        }
    }

    #[must_use]
    pub fn with_background_manager(mut self, background: Arc<BackgroundProcessManager>) -> Self {
        self.background = Some(background);
        self
    }

    #[must_use]
    pub fn with_command_safety(mut self, safety: Arc<CommandSafetyClassifier>) -> Self {
        self.safety = safety;
        self
    }

    async fn run_foreground(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        self.foreground.settle_abandoned().await?;
        cancellation.check()?;
        let (completed, completion) = tokio::sync::watch::channel(false);
        let command = Arc::new(ForegroundCommand {
            _executor: Arc::clone(&self.executor),
            cancellation: cancellation.clone(),
            abandoned: std::sync::atomic::AtomicBool::new(false),
            completion,
        });
        self.foreground
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::clone(&command));
        let mut caller = ForegroundCaller {
            command,
            armed: true,
        };
        let executor = Arc::clone(&self.executor);
        let foreground = Arc::clone(&self.foreground);
        let finished_command = Arc::clone(&caller.command);
        let invocation_executor = Arc::clone(&executor);
        let invocation =
            tokio::spawn(
                async move { invocation_executor.run(request, cancellation, output).await },
            );
        let task = tokio::spawn(async move {
            let result = invocation
                .await
                .map_err(|error| {
                    ToolError::Command(format!("foreground command task failed: {error}"))
                })
                .and_then(std::convert::identity);
            executor.settle_effects().await.map_err(|error| {
                ToolError::EffectsUnsettled(format!("foreground command cleanup failed: {error}"))
            })?;
            foreground
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|call| !Arc::ptr_eq(call, &finished_command));
            completed.send_replace(true);
            result
        });
        let result = task.await.map_err(|error| {
            ToolError::EffectsUnsettled(format!("foreground command cleanup owner failed: {error}"))
        })?;
        caller.armed = matches!(&result, Err(ToolError::EffectsUnsettled(_)));
        result
    }
}

#[async_trait]
impl Tool for BashTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        self.foreground.settle_abandoned().await?;
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "bash".to_owned(),
            description: "Run a sandboxed shell command with live stdout/stderr streaming, or supervise it in the background."
                .to_owned(),
            input_schema: input_schema::<BashInput>(),
            capabilities: CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::Network,
                ToolCapability::Execute,
            ]),
        }
    }

    fn behavior(&self) -> crate::ToolBehavior {
        crate::ToolBehavior::Shell
    }

    fn invocation_capabilities(&self, input: &Value) -> Result<CapabilityManifest, ToolError> {
        let input: BashInput = parse_input(input.clone())?;
        Ok(if input.run_in_background {
            CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::Network,
                ToolCapability::Execute,
            ])
        } else {
            CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
                ToolCapability::Network,
                ToolCapability::Execute,
            ])
        })
    }

    fn mutation_scope(&self, input: &Value) -> crate::MutationScope {
        serde_json::from_value::<BashInput>(input.clone()).map_or(
            crate::MutationScope::OpaqueWorkspace,
            |input| {
                if input.run_in_background {
                    crate::MutationScope::None
                } else {
                    crate::MutationScope::OpaqueWorkspace
                }
            },
        )
    }

    async fn end_session(&self, session_id: &rw_types::SessionId) -> Result<(), ToolError> {
        if let Some(background) = &self.background {
            background.shutdown_session(session_id).await?;
        }
        Ok(())
    }

    fn session_activity(&self, session_id: &rw_types::SessionId) -> Option<String> {
        self.background
            .as_ref()
            .is_some_and(|background| background.has_running(session_id))
            .then(|| "background shell process is still running".to_owned())
    }

    fn observes_session_resources(&self) -> bool {
        true
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: BashInput = parse_input(input)?;
        if !input.run_in_background && input.sandbox == BashSandboxMode::ReadOnly {
            return Err(ToolError::InvalidInput(
                "read_only sandbox mode is reserved for supervised background commands".to_owned(),
            ));
        }
        if input.command.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "command must not be empty".to_owned(),
            ));
        }
        let cwd = context.resolve_existing(&input.cwd)?;
        if !cwd.is_dir() {
            return Err(ToolError::InvalidInput(
                "command cwd must be a directory".to_owned(),
            ));
        }
        let framing_reserve = self.limits.max_result_bytes.min(512) / 4;
        let capture = Arc::new(CapturingSink::new(
            Arc::clone(&context.output),
            self.limits.max_result_bytes.saturating_sub(framing_reserve),
        ));
        let network_domains = normalize_requested_domains(&input.network_domains)?;
        let request = CommandRequest {
            network_domains,
            command: input.command,
            cwd,
            env: input.env,
            sandbox: input.sandbox,
        };
        if input.run_in_background {
            if input.sandbox != BashSandboxMode::Sandboxed {
                return Err(ToolError::InvalidInput(
                    "background commands must use the write-denied sandbox".to_owned(),
                ));
            }
            let mut request = request;
            request.sandbox = BashSandboxMode::ReadOnly;
            let manager = self.background.as_ref().ok_or_else(|| {
                ToolError::Command("background process manager is unavailable".to_owned())
            })?;
            let session_id = context.session_id().ok_or_else(|| {
                ToolError::Command("background commands require an actor-owned session".to_owned())
            })?;
            let process = manager.start(Arc::clone(&self.executor), session_id, request)?;
            if context.cancellation.is_cancelled() {
                let _ = manager.kill(session_id, &process.process_id).await;
                return Err(ToolError::Cancelled);
            }
            return Ok(ToolResult::new(
                format!("background process started: {}", process.process_id),
                json!({ "background_process": process }),
            )
            .with_presentation(BACKGROUND_START_PRESENTATION.plan()?));
        }
        let outcome = self
            .run_foreground(request, context.cancellation.clone(), capture.clone())
            .await?;
        context.cancellation.check()?;
        let captured = capture.finish()?;
        let model_text = format!(
            "exit code: {}\nstdout:\n{}\nstderr:\n{}",
            outcome.exit_code, captured.stdout, captured.stderr
        );
        let mut result = ToolResult::new(
            model_text,
            json!({
                "exit_code": outcome.exit_code,
                "stdout_truncated": captured.stdout_truncated,
                "stderr_truncated": captured.stderr_truncated,
            }),
        )
        .with_presentation(BASH_PRESENTATION.plan()?);
        result.truncated = captured.stdout_truncated || captured.stderr_truncated;
        Ok(result)
    }
}

fn normalize_requested_domains(domains: &[String]) -> Result<Vec<String>, ToolError> {
    let mut normalized = domains
        .iter()
        .map(|domain| {
            normalize_egress_domain(domain).ok_or_else(|| {
                ToolError::InvalidInput(format!("invalid requested network domain {domain:?}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

mod safety;

mod execution_lease;

mod replay;

mod native;

mod watchdog;

mod output;

mod process_group;

pub use safety::{CommandSafety, CommandSafetyClassifier, classify_safe_command};

pub(crate) use safety::audited_system_git;

pub use execution_lease::ExecutionLease;

mod scratch;
pub use scratch::CommandScratch;

pub use replay::{
    CommandFixtureRedactor, IdentityCommandFixtureRedactor, RecordingCommandExecutor,
    ReplayCommandExecutor,
};

pub use native::TokioCommandExecutor;
pub use process_group::terminate_and_wait_process_group;

use output::CapturingSink;

#[cfg(test)]
mod tests;
