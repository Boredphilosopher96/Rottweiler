use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, miette};
use rw_core::{
    BoundClient, ClientCommand, ClientId, CommandOutcome, EngineEvent, EventClock, HostError,
    HostReadChannel, HostReadResult, HostReply, ProviderApiKey, SequenceId, SessionId,
    SystemEventClock,
};
use rw_runtime::{TranscriptReader, session};

#[cfg(unix)]
use crate::runtime_paths::{
    RuntimeDirectoryGuard, allocate_runtime_paths, locate_js_host_executable,
};
use crate::{server, tui_config};

#[derive(Clone)]
pub(super) struct HistoricalReplayEngine {
    session_id: SessionId,
    reader: Arc<TranscriptReader>,
    reads: HostReadChannel,
    event_budget: rw_core::HostEventBudget,
}

impl HistoricalReplayEngine {
    fn open(storage_root: &Path, session_id: SessionId) -> std::result::Result<Self, HostError> {
        SessionId::validate(&session_id.0)
            .map_err(|error| HostError::Protocol(error.to_string()))?;
        Ok(Self {
            session_id,
            reader: TranscriptReader::open(storage_root)?,
            reads: HostReadChannel::new(256)?,
            event_budget: rw_core::HostEventBudget::default(),
        })
    }

    fn validate_read(&self, command: &ClientCommand) -> std::result::Result<(), HostError> {
        let (ClientCommand::ReadTranscript {
            session_id: session,
            scope,
            ..
        }
        | ClientCommand::ReadTranscriptTail {
            session_id: session,
            scope,
            ..
        }
        | ClientCommand::ReadTranscriptContent {
            session_id: session,
            scope,
            ..
        }
        | ClientCommand::ReadSessionChildren {
            session_id: session,
            scope,
            ..
        }
        | ClientCommand::GetTodos {
            session_id: session,
            scope,
            ..
        }) = command
        else {
            return Err(HostError::Protocol(
                "query is unavailable in historical view".into(),
            ));
        };
        let root = scope
            .root(session)
            .map_err(|message| HostError::Protocol(message.into()))?;
        if root != &self.session_id {
            return Err(HostError::Protocol("historical read root mismatch".into()));
        }
        Ok(())
    }

    async fn query(
        &self,
        command: ClientCommand,
    ) -> std::result::Result<HostReadResult, HostError> {
        self.validate_read(&command)?;
        let meta = rw_core::CommandAckMeta {
            protocol_version: rw_core::PROTOCOL_VERSION,
            client_id: command.meta().client_id.clone(),
            request_id: command.meta().request_id.clone(),
            emitted_at: SystemEventClock.emitted_at(),
        };
        let event = match command {
            ClientCommand::ReadSessionChildren {
                session_id, scope, ..
            } => {
                let result = self.reader.children(session_id.clone(), scope).await?;
                return Ok(
                    result.into_query(|result| EngineEvent::SessionChildrenReady {
                        meta,
                        session_id,
                        result,
                    }),
                );
            }
            ClientCommand::GetTodos {
                session_id, scope, ..
            } => EngineEvent::TodosRead {
                meta,
                result: self.reader.todos(session_id.clone(), scope).await?,
                session_id,
            },
            ClientCommand::ReadTranscriptTail {
                session_id,
                scope,
                read,
                ..
            } => {
                let result = self.reader.tail(session_id.clone(), scope, read).await?;
                return Ok(
                    result.into_query(|result| EngineEvent::TranscriptTailReady {
                        meta,
                        session_id,
                        result,
                    }),
                );
            }
            ClientCommand::ReadTranscript {
                session_id,
                scope,
                read,
                ..
            } => {
                let result = self.reader.page(session_id.clone(), scope, read).await?;
                EngineEvent::TranscriptPageReady {
                    meta,
                    session_id,
                    result,
                }
            }
            ClientCommand::ReadTranscriptContent {
                session_id,
                scope,
                read,
                ..
            } => {
                let page = self.reader.content(session_id.clone(), scope, read).await?;
                EngineEvent::TranscriptContentReady {
                    meta,
                    session_id,
                    page,
                }
            }
            _ => {
                return Err(HostError::Protocol(
                    "query is unavailable in historical view".into(),
                ));
            }
        };
        Ok(HostReadResult::new(
            CommandOutcome::Accepted {},
            vec![event],
            (),
        ))
    }
}

#[async_trait]
impl server::ServerEngine for HistoricalReplayEngine {
    async fn dispatch(
        &self,
        bound_client: ClientId,
        command: ClientCommand,
    ) -> std::result::Result<HostReply, String> {
        if matches!(&command, ClientCommand::AttachSession {
            meta, session_id, role: rw_core::ClientRole::Observer, ..
        } if session_id == &self.session_id && meta.protocol_version == rw_core::PROTOCOL_VERSION
            && meta.request_id.is_valid())
        {
            return Ok(HostReply::command(CommandOutcome::Accepted {}));
        }
        Ok(self
            .reads
            .dispatch(
                BoundClient {
                    client_id: bound_client,
                },
                command,
                |command| self.query(command),
            )
            .await)
    }

    async fn subscribe(
        &self,
        bound_client: ClientId,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<rw_core::HostEvent, String>>,
        server::EventSubscriptionError,
    > {
        if session_id.as_ref() != Some(&self.session_id) {
            return Err(server::EventSubscriptionError::Other(
                "historical session mismatch".into(),
            ));
        }
        let bootstrap = self
            .reader
            .bootstrap(self.session_id.clone())
            .await
            .map_err(|error| server::EventSubscriptionError::Other(error.to_string()))?;
        if last_seen.is_some_and(|seen| bootstrap.through_sequence.is_none_or(|tail| seen > tail)) {
            return Err(server::EventSubscriptionError::ReplayCursorAhead);
        }
        let session_id = self.session_id.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let event_budget = self.event_budget.clone();
        tokio::spawn(async move {
            if let Some(created) = bootstrap.created
                && last_seen.is_none()
                && sender
                    .send(
                        event_budget
                            .encode(&created)
                            .await
                            .map_err(|error| error.to_string()),
                    )
                    .await
                    .is_err()
            {
                return;
            }
            let ready = EngineEvent::SessionHistoryReady {
                meta: rw_core::CommandAckMeta {
                    protocol_version: rw_core::PROTOCOL_VERSION,
                    client_id: bound_client,
                    request_id: rw_core::RequestId("historical-view".into()),
                    emitted_at: SystemEventClock.emitted_at(),
                },
                session_id,
                through_sequence: bootstrap.through_sequence,
            };
            if sender
                .send(
                    event_budget
                        .encode(&ready)
                        .await
                        .map_err(|error| error.to_string()),
                )
                .await
                .is_ok()
            {
                sender.closed().await;
            }
        });
        Ok(receiver)
    }

    async fn complete_shell(
        &self,
        _session_id: SessionId,
        _shell_id: rw_core::ShellId,
        _status: i32,
        _captured_output: Option<String>,
    ) -> std::result::Result<(), String> {
        Err("historical view is read-only".into())
    }
    async fn submit_provider_api_key(
        &self,
        _bound_client: ClientId,
        _session_id: SessionId,
        _provider: String,
        _api_key: ProviderApiKey,
    ) -> std::result::Result<rw_core::ProviderApiKeySubmission, String> {
        Err("historical view is read-only".into())
    }
    async fn activate_provider(
        &self,
        _bound_client: ClientId,
        _session_id: SessionId,
        _provider: String,
    ) -> std::result::Result<(), String> {
        Err("historical view is read-only".into())
    }
}

pub(super) async fn run_history_replay(storage_root: &Path, session: &str) -> Result<()> {
    let tui = locate_js_host_executable()?;
    run_history_replay_with_tui(storage_root, session, &tui).await
}

pub(super) async fn run_history_replay_with_tui(
    storage_root: &Path,
    session: &str,
    tui: &Path,
) -> Result<()> {
    let storage_root = storage_root.to_owned();
    let session = session.to_owned();
    let tui = tui.to_owned();
    crate::tui_session::run(move |stop| {
        Box::pin(run_history_session(storage_root, session, tui, stop))
    })
    .await
}

async fn run_history_session(
    storage_root: std::path::PathBuf,
    session: String,
    tui: std::path::PathBuf,
    mut stop: crate::tui_session::Stop,
) -> Result<()> {
    if stop.requested() {
        return Ok(());
    }
    let session = session.as_str();
    let (user_home, user_rottweiler) =
        session::extension_user_roots(&storage_root.join("credentials.toml"));
    let keybindings = tui_config::load_keybindings(None, None, &user_home, &user_rottweiler)
        .map_err(|error| miette!(error.to_string()))?;
    let engine = Arc::new(
        HistoricalReplayEngine::open(&storage_root, SessionId(session.to_owned()))
            .map_err(|error| miette!(error.to_string()))?,
    );
    engine
        .reader
        .bootstrap(engine.session_id.clone())
        .await
        .map_err(|error| miette!(error.to_string()))?;
    if stop.requested() {
        return Ok(());
    }
    let paths = allocate_runtime_paths(&storage_root)?;
    let mut runtime_directory = RuntimeDirectoryGuard::capture(&paths.directory)?;
    let (runtime, listener) = server::ServerRuntime::create_for_session(paths, Some(session))?;
    let state = server::ServerState::new(engine, &runtime);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut server_task = tokio::spawn(server::serve(listener, state, shutdown_rx));
    let cursor = runtime.paths.directory.join("last-seen");
    let fork_operation_directory = storage_root.join("control/pending-forks");
    let mut client = crate::tui_launch::TuiProcess::start(&crate::tui_launch::TuiLaunch {
        executable: &tui,
        socket: &runtime.paths.socket,
        token_file: &runtime.paths.token,
        session_id: session,
        last_seen_file: &cursor,
        fork_operation_directory: &fork_operation_directory,
        keybindings: keybindings.as_deref(),
        theme: "",
        replay: true,
    });
    let mut server_finished = false;
    let result = tokio::select! {
        result = client.wait() => result.into_diagnostic(),
        () = stop.cancelled() => Ok(()),
        result = &mut server_task => {
            server_finished = true;
            match result {
                Ok(Ok(())) => Err(miette!("historical replay server stopped unexpectedly")),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error).into_diagnostic(),
            }
        }
    };
    let cleanup = client.shutdown().await.into_diagnostic();
    let _ = shutdown.send(true);
    let server_cleanup = if server_finished {
        Ok(())
    } else {
        server_task
            .await
            .into_diagnostic()
            .and_then(|result| result)
    };
    if cleanup.is_err() || server_cleanup.is_err() {
        runtime_directory.preserve();
        tracing::warn!(path = %runtime.paths.directory.display(), "retained replay runtime because cleanup failed");
    }
    drop(runtime);
    result.and(cleanup).and(server_cleanup)?;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lifecycle_tests;
