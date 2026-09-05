#![allow(clippy::expect_used)]
use std::{
    collections::HashSet,
    sync::{
        Condvar,
        atomic::{AtomicU8, AtomicUsize},
    },
    time::{Duration, Instant},
};

use futures_util::stream;
use rw_ext::{
    CommandDescriptor as ExtensionCommandDescriptor, CommandExecutionError, CommandHandler,
    CommandInvocation,
};
use rw_providers::{BoxEventStream, ProviderRequest};
use rw_tools::ToolRegistry;
use rw_types::{
    AttachmentData, CommandMeta, PROTOCOL_VERSION,
    config::{PermissionDecision, ThinkingLevel},
};
use tempfile::TempDir;
use tokio::sync::Notify;

use super::*;
use crate as rw_core_batch;
use crate::{
    ModelDriver, NoopFolderTrustController, NoopMutationCheckpointCoordinator, NoopSecretRedactor,
    NoopSessionEventSink, PermissionGate, SessionActor, SessionActorConfig, SessionCommandAction,
    SessionCommandContext, SessionCommandOutput, SessionEventSink, SessionRecoveredState,
    builtin_command_registry, builtin_hook_dispatcher,
};

struct IdleModel;

struct ActivatableModel;

struct SummaryModel;

struct MarkerCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for MarkerCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        _invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: "marker".to_owned(),
            action: SessionCommandAction::None,
        })
    }
}

#[async_trait::async_trait]
impl ModelDriver for IdleModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Ok(Box::pin(stream::empty()))
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        matches!(alias, "fast" | "big") || alias.contains('/')
    }
}

#[async_trait::async_trait]
impl ModelDriver for SummaryModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Ok(Box::pin(stream::iter([
            Ok(rw_providers::ProviderEvent::TextDelta {
                text: "durable model handoff".to_owned(),
            }),
            Ok(rw_providers::ProviderEvent::Finished {
                reason: rw_providers::FinishReason::Stop,
            }),
        ])))
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        matches!(alias, "fast" | "big") || alias.contains('/')
    }
}

#[async_trait]
impl ModelDriver for ActivatableModel {
    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Ok(Box::pin(stream::empty()))
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        matches!(alias, "fast" | "big") || alias.contains('/')
    }

    async fn activate_provider(
        &self,
        _provider: &str,
        _selected_model: Option<&str>,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

struct StubFactory {
    root: TempDir,
    next: AtomicUsize,
    resumes: AtomicUsize,
    fail_resume_once: AtomicBool,
    corrupt_identity: bool,
    panic_resume: bool,
    block_create: AtomicBool,
    block_resume: AtomicBool,
    block_fork: AtomicBool,
    create_started: Notify,
    create_release: Notify,
    resume_started: Notify,
    resume_release: Notify,
    fork_started: Notify,
    fork_release: Notify,
    fork_turns: Mutex<Vec<TurnId>>,
    shutdowns: AtomicUsize,
    event_sink: Option<Arc<dyn SessionEventSink>>,
    model: Arc<dyn ModelDriver>,
}

impl StubFactory {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("host test root"),
            next: AtomicUsize::new(1),
            resumes: AtomicUsize::new(0),
            fail_resume_once: AtomicBool::new(false),
            corrupt_identity: false,
            panic_resume: false,
            block_create: AtomicBool::new(false),
            block_resume: AtomicBool::new(false),
            block_fork: AtomicBool::new(false),
            create_started: Notify::new(),
            create_release: Notify::new(),
            resume_started: Notify::new(),
            resume_release: Notify::new(),
            fork_started: Notify::new(),
            fork_release: Notify::new(),
            fork_turns: Mutex::new(Vec::new()),
            shutdowns: AtomicUsize::new(0),
            event_sink: None,
            model: Arc::new(IdleModel),
        }
    }

    fn with_event_sink(event_sink: Arc<dyn SessionEventSink>) -> Self {
        Self {
            event_sink: Some(event_sink),
            ..Self::new()
        }
    }

    fn with_model(model: Arc<dyn ModelDriver>) -> Self {
        Self {
            model,
            ..Self::new()
        }
    }

    fn session(&self, session_id: &SessionId) -> HostedSession {
        let workspace = self.root.path().join(&session_id.0);
        std::fs::create_dir_all(&workspace).expect("session workspace");
        let mut commands = builtin_command_registry().expect("commands");
        commands
            .register(
                ExtensionCommandDescriptor::new(
                    format!("only.{}", session_id.0),
                    "session-specific command",
                ),
                MarkerCommand,
            )
            .expect("session marker command");
        let handle = SessionActor::spawn(SessionActorConfig {
            budget_session_id: session_id.clone(),
            session_id: session_id.clone(),
            workspace_root: workspace,
            additional_workspace_roots: Vec::new(),
            workspace_generation: 0,
            initial_session_context: Vec::new(),
            startup_notifications: Vec::new(),
            model_alias: "fast".to_owned(),
            model: Arc::clone(&self.model),
            tools: Arc::new(ToolRegistry::new()),
            permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
            hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
            commands: Arc::new(commands),
            modes: Arc::new(rw_ext::ModeRegistry::builtins().expect("built-in modes")),
            event_sink: self
                .event_sink
                .clone()
                .unwrap_or_else(|| Arc::new(NoopSessionEventSink::default())),
            event_clock: Arc::new(SystemEventClock),
            provider_admission: crate::provider_admission::testing::admission(),
            secret_redactor: Arc::new(NoopSecretRedactor),
            checkpoints: Arc::new(NoopMutationCheckpointCoordinator),
            folder_trust: Arc::new(NoopFolderTrustController),
            workspace_roots: Arc::new(crate::NoopWorkspaceRootController),
            extension_development: Arc::new(crate::NoopSessionExtensionController),
            resources: Arc::new(crate::NoopSessionResources),
            recovered: SessionRecoveredState::default(),
            max_turns: 2,
            identical_tool_failure_limit: 2,
            max_output_tokens: 128,
            thinking: ThinkingLevel::Off,
            event_capacity: 64,
        })
        .expect("session actor");
        HostedSession::new(
            SessionDescriptor {
                session_id: if self.corrupt_identity {
                    SessionId("wrong-identity".to_owned())
                } else {
                    session_id.clone()
                },
                title: "New session".to_owned(),
                workspace_name: session_id.0.clone(),
                model: ModelAlias("fast".to_owned()),
                driver_client_id: None,
                shell_active: false,
            },
            handle,
        )
    }
}

#[async_trait]
impl SessionFactory for StubFactory {
    fn allocate_session_id(&self) -> Result<SessionId, HostError> {
        Ok(SessionId(format!(
            "created-{}",
            self.next.fetch_add(1, Ordering::Relaxed)
        )))
    }

    async fn create(&self, request: CreateSessionRequest) -> Result<HostedSession, HostError> {
        if self.block_create.load(Ordering::Acquire) {
            self.create_started.notify_one();
            self.create_release.notified().await;
        }
        Ok(self.session(&request.session_id))
    }

    async fn resume(&self, session_id: &SessionId) -> Result<HostedSession, HostError> {
        assert!(!self.panic_resume, "injected factory panic");
        self.resumes.fetch_add(1, Ordering::Relaxed);
        if self.block_resume.load(Ordering::Acquire) {
            self.resume_started.notify_one();
            self.resume_release.notified().await;
        } else {
            tokio::task::yield_now().await;
        }
        if self.fail_resume_once.swap(false, Ordering::AcqRel) {
            return Err(HostError::Persistence("injected resume failure".to_owned()));
        }
        Ok(self.session(session_id))
    }

    async fn fork(&self, request: ForkSessionRequest) -> Result<HostedSession, HostError> {
        self.fork_turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.at_turn.clone());
        if self.block_fork.load(Ordering::Acquire) {
            self.fork_started.notify_one();
            self.fork_release.notified().await;
        }
        Ok(self.session(&request.child_session_id))
    }

    async fn shutdown(&self) -> Result<(), HostError> {
        self.shutdowns.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

const BLOCK_MODEL: u8 = 1;
const BLOCK_SHELL_ACTIVE: u8 = 2;
const BLOCK_SHELL_INACTIVE: u8 = 3;

#[derive(Default)]
struct BlockingDescriptorSink {
    inner: Arc<NoopSessionEventSink>,
    block: AtomicU8,
    append_started: Notify,
    append_release: Notify,
}

impl BlockingDescriptorSink {
    fn block(&self, target: u8) {
        self.block.store(target, Ordering::Release);
    }

    fn release(&self) {
        self.append_release.notify_one();
    }

    fn event_target(event: &EngineEvent) -> u8 {
        match event {
            EngineEvent::ModelChanged { .. } => BLOCK_MODEL,
            EngineEvent::UserShellStateChanged { active: true, .. } => BLOCK_SHELL_ACTIVE,
            EngineEvent::UserShellStateChanged { active: false, .. } => BLOCK_SHELL_INACTIVE,
            _ => 0,
        }
    }
}

#[async_trait]
impl SessionEventSink for BlockingDescriptorSink {
    async fn extension_state(
        &self,
        _plugin_id: &str,
    ) -> Result<crate::engine::ExtensionStateView, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "this ephemeral event sink does not provide durable extension state".to_owned(),
        ))
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
    async fn reserve(
        &self,
        _plan: &rw_core_batch::EventBatchPlan,
    ) -> Result<rw_core_batch::EventBatchReservation, AgentLoopError> {
        Ok(rw_core_batch::EventBatchReservation::new(()))
    }

    async fn commit(
        self: Arc<Self>,
        batch: Arc<rw_core_batch::AdmittedEventBatch>,
    ) -> Result<Arc<rw_core_batch::AdmittedEventBatch>, AgentLoopError> {
        for event in batch.events() {
            let target = Self::event_target(event);
            if target != 0 && target == self.block.load(Ordering::Acquire) {
                self.append_started.notify_one();
                self.append_release.notified().await;
            }
        }
        Arc::clone(&self.inner).commit(batch).await
    }

    fn capture_read_view(&self) -> Result<Arc<dyn crate::SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }
}

#[derive(Default)]
struct StubQueries {
    auth: Option<Arc<AuthFixture>>,
    persisted_models: std::sync::Mutex<Vec<String>>,
    exports: std::sync::Mutex<Vec<(SessionId, TranscriptFormat, String, bool)>>,
    fail_model_catalog: bool,
    fail_model_persistence: bool,
}

struct AuthFixture {
    completion: watch::Sender<bool>,
    cancelled: Arc<AtomicBool>,
    persistence: Option<Arc<BlockingCredentialMutation>>,
}

impl AuthFixture {
    fn pending() -> Arc<Self> {
        let (completion, _) = watch::channel(false);
        Arc::new(Self {
            completion,
            cancelled: Arc::new(AtomicBool::new(false)),
            persistence: None,
        })
    }

    fn with_persistence(persistence: Arc<BlockingCredentialMutation>) -> Arc<Self> {
        let (completion, _) = watch::channel(false);
        Arc::new(Self {
            completion,
            cancelled: Arc::new(AtomicBool::new(false)),
            persistence: Some(persistence),
        })
    }
}

struct BlockingCredentialMutation {
    started: Notify,
    persisted: AtomicBool,
    gate: (Mutex<bool>, Condvar),
}

impl BlockingCredentialMutation {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            persisted: AtomicBool::new(false),
            gate: (Mutex::new(false), Condvar::new()),
        })
    }

    #[allow(clippy::unnecessary_wraps)]
    fn run(&self) -> Result<Vec<String>, HostError> {
        self.started.notify_one();
        let (gate, release) = &self.gate;
        let mut open = gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*open {
            open = release
                .wait(open)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        self.persisted.store(true, Ordering::Release);
        Ok(Vec::new())
    }

    fn release(&self) {
        let (gate, release) = &self.gate;
        *gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        release.notify_all();
    }
}

#[async_trait]
impl HostQueryService for StubQueries {
    async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError> {
        Ok(vec![CommandDescriptor {
            name: "help".to_owned(),
            description: "Show help".to_owned(),
            usage: "/help".to_owned(),
            source: rw_types::CommandSource::default(),
        }])
    }

    async fn model_catalog(
        &self,
        _refresh: bool,
        _selected_model: Option<&str>,
        _resolved_model: Option<&str>,
    ) -> Result<ModelCatalogSnapshot, HostError> {
        if self.fail_model_catalog {
            return Err(HostError::Query(
                "injected provider catalog refresh failure".to_owned(),
            ));
        }
        Ok(ModelCatalogSnapshot {
            aliases: Vec::new(),
            models: Vec::new(),
            providers: Vec::new(),
            cached: false,
            truncated: false,
        })
    }

    async fn persist_project_model_selection(
        &self,
        _session: &SessionDescriptor,
        model: &ModelAlias,
    ) -> Result<(), HostError> {
        if self.fail_model_persistence {
            return Err(HostError::Query(
                "injected project model persistence failure".to_owned(),
            ));
        }
        self.persisted_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(model.0.clone());
        Ok(())
    }

    async fn begin_provider_auth(&self, provider: &str) -> Result<ProviderAuthAttempt, HostError> {
        let fixture = self
            .auth
            .clone()
            .ok_or_else(|| HostError::Query("provider authentication is unavailable".to_owned()))?;
        let mut completion = fixture.completion.subscribe();
        let completion_provider = provider.to_owned();
        let persistence = fixture.persistence.clone();
        let future = Box::pin(async move {
            while !*completion.borrow_and_update() {
                completion.changed().await.map_err(|_| {
                    HostError::Query("provider authentication cancelled".to_owned())
                })?;
            }
            let completion = ProviderAuthCompletion::new(
                completion_provider,
                "provider authentication completed".to_owned(),
                Vec::new(),
            );
            Ok(if let Some(persistence) = persistence {
                completion.with_persistence(move || persistence.run())
            } else {
                completion
            })
        });
        let cancellation = Arc::clone(&fixture.cancelled);
        let cancel_signal = fixture.completion.clone();
        Ok(ProviderAuthAttempt::new(
            ProviderAuthChallenge::DeviceFlow {
                verification_uri: "https://example.test/device".to_owned(),
                user_code: "ABCD-1234".to_owned(),
            },
            Vec::new(),
            future,
            Arc::new(move || {
                cancellation.store(true, Ordering::Release);
                let _ = cancel_signal.send(true);
            }),
        ))
    }

    async fn search_workspace_files(
        &self,
        _session: &SessionDescriptor,
        _query: &str,
        _limit: u32,
    ) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
        Ok((Vec::new(), false))
    }

    async fn preview_workspace_file(
        &self,
        _session: &SessionDescriptor,
        path: &str,
        _max_bytes: u32,
    ) -> Result<WorkspaceFilePreview, HostError> {
        Ok(WorkspaceFilePreview {
            path: path.to_owned(),
            media_type: "text/plain".to_owned(),
            data: AttachmentData::Text {
                content: String::new(),
            },
            total_bytes: 0,
            truncated: false,
        })
    }

    async fn workspace_status(
        &self,
        session: &SessionDescriptor,
    ) -> Result<WorkspaceStatus, HostError> {
        Ok(WorkspaceStatus {
            workspace_name: session.workspace_name.clone(),
            branch: None,
            changed_paths: Vec::new(),
            truncated: false,
        })
    }

    async fn workspace_diff(
        &self,
        _session: &SessionDescriptor,
        path: &str,
        _max_bytes: u32,
    ) -> Result<WorkspaceDiff, HostError> {
        Ok(WorkspaceDiff {
            path: path.to_owned(),
            unified_diff: String::new(),
            truncated: false,
            binary: false,
        })
    }

    async fn export_session(
        &self,
        session: &SessionDescriptor,
        format: TranscriptFormat,
        output_path: &str,
        force: bool,
    ) -> Result<String, HostError> {
        self.exports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                session.session_id.clone(),
                format,
                output_path.to_owned(),
                force,
            ));
        Ok(output_path.to_owned())
    }
}

fn meta(client: &str, request: &str) -> CommandMeta {
    CommandMeta {
        protocol_version: PROTOCOL_VERSION,
        client_id: ClientId(client.to_owned()),
        request_id: RequestId(request.to_owned()),
    }
}

fn host(max_sessions: usize) -> (EngineHost, Arc<StubFactory>) {
    let factory = Arc::new(StubFactory::new());
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions,
            max_deduplicated_requests: 32,
        },
        factory.clone(),
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    (host, factory)
}

mod catalog;
mod closure;
mod delivery;
mod models;
mod provider_auth;
mod session_queries;
mod sessions;
mod startup;
