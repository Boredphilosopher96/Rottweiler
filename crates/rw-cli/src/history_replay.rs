use std::{collections::HashSet, path::Path, sync::Arc};

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, miette};
use rw_core::{
    ClientCommand, ClientId, CommandOutcome, EngineEvent, ProviderApiKey, SequenceId, SessionId,
};
use rw_runtime::{session, session_history};

#[cfg(unix)]
use crate::runtime_paths::{RuntimeDirectoryGuard, allocate_runtime_paths, locate_tui_executable};
use crate::{server, tui_config};

#[derive(Clone)]
pub(super) struct HistoricalReplayEngine {
    pub(super) session_id: SessionId,
    pub(super) events: Arc<Vec<HistoricalReplayItem>>,
    pub(super) through_sequence: Option<SequenceId>,
}

#[derive(Clone, Debug)]
pub(super) enum HistoricalReplayItem {
    Durable(rw_store::session::EventEnvelope<EngineEvent>),
    Progress {
        parent_cursor: SequenceId,
        event: EngineEvent,
    },
}

pub(super) const MAX_REPLAY_CHILD_DEPTH: usize = 8;
pub(super) const MAX_REPLAY_CHILD_SESSIONS: usize = 1_024;
pub(super) const MAX_REPLAY_PROGRESS_BYTES: usize = 256 * 1024;

pub(super) struct HistoricalReplayBudget {
    pub(super) bytes: u64,
    pub(super) events: usize,
    pub(super) sessions: usize,
}

impl HistoricalReplayBudget {
    pub(super) fn consume(&mut self, value: &serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(value).into_diagnostic()?;
        if bytes.len() > MAX_REPLAY_PROGRESS_BYTES {
            return Err(miette!("historical child progress exceeds its size limit"));
        }
        let length = u64::try_from(bytes.len()).into_diagnostic()?;
        self.bytes = self
            .bytes
            .checked_sub(length)
            .ok_or_else(|| miette!("historical child replay exceeds its byte limit"))?;
        self.events = self
            .events
            .checked_sub(1)
            .ok_or_else(|| miette!("historical child replay exceeds its event limit"))?;
        Ok(())
    }
}

#[async_trait]
impl server::ServerEngine for HistoricalReplayEngine {
    async fn dispatch(
        &self,
        _bound_client: ClientId,
        command: ClientCommand,
    ) -> std::result::Result<CommandOutcome, String> {
        match command {
            ClientCommand::AttachSession {
                session_id,
                role: rw_core::ClientRole::Observer,
                ..
            } if session_id == self.session_id => Ok(CommandOutcome::Accepted),
            _ => Ok(CommandOutcome::Rejected {
                error: rw_core::EngineError {
                    category: rw_core::EngineErrorCategory::Protocol,
                    code: "historical_replay_read_only".to_owned(),
                    message: "historical replay accepts only observer attachment".to_owned(),
                    retryable: false,
                    details: None,
                },
            }),
        }
    }

    async fn subscribe(
        &self,
        bound_client: ClientId,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<EngineEvent, String>>,
        server::EventSubscriptionError,
    > {
        if session_id.as_ref() != Some(&self.session_id) {
            return Err(server::EventSubscriptionError::Other(
                "historical replay session mismatch".to_owned(),
            ));
        }
        let events = Arc::clone(&self.events);
        let replay_session = self.session_id.clone();
        let through_sequence = self.through_sequence;
        let (sender, receiver) = tokio::sync::mpsc::channel(256);
        tokio::spawn(async move {
            for item in events.iter() {
                let event = match item {
                    HistoricalReplayItem::Durable(envelope)
                        if last_seen.is_none_or(|sequence| envelope.sequence.0 > sequence.0) =>
                    {
                        &envelope.event
                    }
                    HistoricalReplayItem::Progress {
                        parent_cursor,
                        event,
                    } if last_seen.is_none_or(|sequence| parent_cursor.0 > sequence.0) => event,
                    _ => continue,
                };
                if sender.send(Ok(event.clone())).await.is_err() {
                    return;
                }
            }
            let _ = sender
                .send(Ok(EngineEvent::SessionReplayCompleted {
                    meta: rw_core::CommandAckMeta {
                        protocol_version: rw_core::PROTOCOL_VERSION,
                        client_id: bound_client,
                        request_id: rw_core::RequestId("historical-replay".to_owned()),
                        emitted_at: "1970-01-01T00:00:00Z".to_owned(),
                    },
                    session_id: replay_session,
                    through_sequence,
                }))
                .await;
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
        Err("historical replay is read-only".to_owned())
    }

    async fn submit_provider_api_key(
        &self,
        _bound_client: ClientId,
        _session_id: SessionId,
        _provider: String,
        _api_key: ProviderApiKey,
    ) -> std::result::Result<rw_core::ProviderApiKeySubmission, String> {
        Err("historical replay is read-only".to_owned())
    }

    async fn activate_provider(
        &self,
        _bound_client: ClientId,
        _session_id: SessionId,
        _provider: String,
    ) -> std::result::Result<(), String> {
        Err("historical replay is read-only".to_owned())
    }
}

pub(super) async fn run_history_replay(
    storage_root: &Path,
    session: &str,
    events: Vec<rw_store::session::EventEnvelope<EngineEvent>>,
) -> Result<()> {
    let tui = locate_tui_executable()?;
    run_history_replay_with_tui(storage_root, session, events, &tui).await
}

pub(super) async fn run_history_replay_with_tui(
    storage_root: &Path,
    session: &str,
    events: Vec<rw_store::session::EventEnvelope<EngineEvent>>,
    tui: &Path,
) -> Result<()> {
    let (user_home, user_rottweiler) =
        session::extension_user_roots(&storage_root.join("credentials.toml"));
    let keybindings = tui_config::load_keybindings(None, None, &user_home, &user_rottweiler)
        .map_err(|error| miette!(error.to_string()))?;
    let through_sequence = events.last().map(|envelope| envelope.sequence);
    let events = historical_replay_items(storage_root, session, events)?;
    let paths = allocate_runtime_paths(storage_root)?;
    let _runtime_directory = RuntimeDirectoryGuard::capture(&paths.directory)?;
    let (runtime, listener) = server::ServerRuntime::create_for_session(paths, Some(session))?;
    let state = server::ServerState::new(
        Arc::new(HistoricalReplayEngine {
            session_id: SessionId(session.to_owned()),
            events: Arc::new(events),
            through_sequence,
        }),
        &runtime,
    );
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let server_task = tokio::spawn(server::serve(listener, state, shutdown_rx));
    let mut command = tokio::process::Command::new(tui);
    command
        .env_remove("ROTTWEILER_TUI_KEYBINDINGS")
        .env("ROTTWEILER_ENGINE_SOCKET", &runtime.paths.socket)
        .env("ROTTWEILER_ENGINE_TOKEN_FILE", &runtime.paths.token)
        .env("ROTTWEILER_SESSION_ID", session)
        .env("ROTTWEILER_REPLAY_MODE", "1")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    if let Some(keybindings) = keybindings {
        command.env("ROTTWEILER_TUI_KEYBINDINGS", keybindings);
    }
    let status = command.status().await;
    let _ = shutdown.send(true);
    server_task.await.into_diagnostic()??;
    drop(runtime);
    let status = status.into_diagnostic()?;
    if !status.success() {
        return Err(miette!("historical replay TUI exited with status {status}"));
    }
    Ok(())
}

pub(super) fn historical_replay_items(
    storage_root: &Path,
    session: &str,
    events: Vec<rw_store::session::EventEnvelope<EngineEvent>>,
) -> Result<Vec<HistoricalReplayItem>> {
    let mut output = Vec::new();
    let mut budget = HistoricalReplayBudget {
        bytes: session_history::MAX_HISTORY_BYTES,
        events: session_history::MAX_HISTORY_EVENTS,
        sessions: MAX_REPLAY_CHILD_SESSIONS,
    };
    let root_session = SessionId(session.to_owned());
    let mut ancestors = HashSet::from([root_session.clone()]);
    for envelope in events {
        let cursor = envelope.sequence;
        let spawned = match &envelope.event {
            EngineEvent::SubagentSpawned {
                subagent_id,
                child_session_id,
                ..
            } => Some((subagent_id.clone(), child_session_id.clone())),
            _ => None,
        };
        output.push(HistoricalReplayItem::Durable(envelope));
        if let Some((subagent_id, child_session_id)) = spawned {
            let child = historical_child_stream(
                storage_root,
                &child_session_id,
                &mut budget,
                &mut ancestors,
                1,
            )?;
            for (child_sequence, event) in child {
                let event = EngineEvent::SubagentProgress {
                    parent_session_id: root_session.clone(),
                    subagent_id: subagent_id.clone(),
                    child_session_id: child_session_id.clone(),
                    child_sequence,
                    event,
                };
                budget.consume(&serde_json::to_value(&event).into_diagnostic()?)?;
                output.push(HistoricalReplayItem::Progress {
                    parent_cursor: cursor,
                    event,
                });
            }
        }
    }
    Ok(output)
}

pub(super) fn historical_child_stream(
    storage_root: &Path,
    session: &SessionId,
    budget: &mut HistoricalReplayBudget,
    ancestors: &mut HashSet<SessionId>,
    depth: usize,
) -> Result<Vec<(Option<SequenceId>, serde_json::Value)>> {
    if depth > MAX_REPLAY_CHILD_DEPTH {
        return Err(miette!("historical child replay exceeds its nesting limit"));
    }
    budget.sessions = budget
        .sessions
        .checked_sub(1)
        .ok_or_else(|| miette!("historical child replay exceeds its session limit"))?;
    if !ancestors.insert(session.clone()) {
        return Err(miette!("historical child replay contains a session cycle"));
    }
    let result = (|| {
        let events = rw_store::session::SessionEventLog::load_existing_bounded::<EngineEvent>(
            storage_root,
            &session.0,
            budget.bytes,
            budget.events,
        )
        .map_err(|error| miette!("historical child session could not be read: {error}"))?;
        let mut output = Vec::new();
        for envelope in events {
            let meta = envelope
                .event
                .meta()
                .ok_or_else(|| miette!("historical child log contains a non-durable event"))?;
            if meta.session_id != *session || meta.sequence_id != envelope.sequence {
                return Err(miette!(
                    "historical child event identity does not match its durable envelope"
                ));
            }
            let spawned = match &envelope.event {
                EngineEvent::SubagentSpawned {
                    subagent_id,
                    child_session_id,
                    ..
                } => Some((subagent_id.clone(), child_session_id.clone())),
                _ => None,
            };
            let value = serde_json::to_value(&envelope.event).into_diagnostic()?;
            budget.consume(&value)?;
            output.push((Some(envelope.sequence), value));
            if let Some((subagent_id, child_session_id)) = spawned {
                for (child_sequence, event) in historical_child_stream(
                    storage_root,
                    &child_session_id,
                    budget,
                    ancestors,
                    depth + 1,
                )? {
                    let progress = EngineEvent::SubagentProgress {
                        parent_session_id: session.clone(),
                        subagent_id: subagent_id.clone(),
                        child_session_id: child_session_id.clone(),
                        child_sequence,
                        event,
                    };
                    let value = serde_json::to_value(progress).into_diagnostic()?;
                    budget.consume(&value)?;
                    output.push((None, value));
                }
            }
        }
        Ok(output)
    })();
    ancestors.remove(session);
    result
}

#[cfg(test)]
mod historical_replay_tests {
    #![allow(clippy::expect_used)]

    use std::fs;

    use super::*;
    use rw_core::{ClientRole, CommandMeta, EventMeta, RequestId};
    use rw_store::session::SessionEventLog;
    use rw_types::SubagentId;

    fn meta() -> CommandMeta {
        CommandMeta {
            protocol_version: rw_core::PROTOCOL_VERSION,
            client_id: ClientId("client".to_owned()),
            request_id: RequestId("request".to_owned()),
        }
    }

    fn event_meta(session: &str, sequence: u64) -> EventMeta {
        EventMeta {
            protocol_version: rw_core::PROTOCOL_VERSION,
            session_id: SessionId(session.to_owned()),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        }
    }

    fn engine() -> HistoricalReplayEngine {
        let session_id = SessionId("history".to_owned());
        HistoricalReplayEngine {
            session_id: session_id.clone(),
            events: Arc::new(vec![HistoricalReplayItem::Durable(
                rw_store::session::EventEnvelope {
                    schema_version: 1,
                    sequence: SequenceId(0),
                    event: EngineEvent::UiNotification {
                        meta: EventMeta {
                            protocol_version: rw_core::PROTOCOL_VERSION,
                            session_id,
                            sequence_id: SequenceId(0),
                            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                            caused_by: None,
                        },
                        plugin_id: "fixture".to_owned(),
                        title: "title".to_owned(),
                        message: "message".to_owned(),
                    },
                },
            )]),
            through_sequence: Some(SequenceId(0)),
        }
    }

    #[tokio::test]
    async fn historical_replay_is_ordered_and_strictly_read_only() {
        let engine = engine();
        let observer = ClientCommand::AttachSession {
            meta: meta(),
            session_id: SessionId("history".to_owned()),
            last_seen_sequence: None,
            role: ClientRole::Observer,
        };
        assert_eq!(
            server::ServerEngine::dispatch(&engine, ClientId("bound".to_owned()), observer).await,
            Ok(CommandOutcome::Accepted)
        );
        let driver = ClientCommand::AttachSession {
            meta: meta(),
            session_id: SessionId("history".to_owned()),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        };
        assert!(matches!(
            server::ServerEngine::dispatch(&engine, ClientId("bound".to_owned()), driver).await,
            Ok(CommandOutcome::Rejected { .. })
        ));
        assert!(matches!(
            server::ServerEngine::dispatch(
                &engine,
                ClientId("bound".to_owned()),
                ClientCommand::Interrupt {
                    meta: meta(),
                    session_id: SessionId("history".to_owned()),
                },
            )
            .await,
            Ok(CommandOutcome::Rejected { .. })
        ));
        assert!(
            server::ServerEngine::subscribe(
                &engine,
                ClientId("bound".to_owned()),
                Some(SessionId("wrong".to_owned())),
                None,
            )
            .await
            .is_err()
        );
        let mut replay = server::ServerEngine::subscribe(
            &engine,
            ClientId("bound".to_owned()),
            Some(SessionId("history".to_owned())),
            None,
        )
        .await
        .expect("subscribe");
        assert!(matches!(
            replay.recv().await,
            Some(Ok(EngineEvent::UiNotification { .. }))
        ));
        assert!(matches!(
            replay.recv().await,
            Some(Ok(EngineEvent::SessionReplayCompleted {
                through_sequence: Some(SequenceId(0)),
                ..
            }))
        ));
        assert!(replay.recv().await.is_none());
    }

    #[test]
    fn historical_replay_rederives_bounded_nested_child_progress() {
        let storage = tempfile::tempdir().expect("storage");
        let mut grandchild =
            SessionEventLog::open(storage.path(), "grandchild").expect("grandchild log");
        grandchild
            .append(EngineEvent::UiNotification {
                meta: event_meta("grandchild", 0),
                plugin_id: "fixture".to_owned(),
                title: "grandchild".to_owned(),
                message: "working".to_owned(),
            })
            .expect("grandchild event");
        let mut child = SessionEventLog::open(storage.path(), "child").expect("child log");
        child
            .append_batch([
                EngineEvent::UiNotification {
                    meta: event_meta("child", 0),
                    plugin_id: "fixture".to_owned(),
                    title: "child".to_owned(),
                    message: "working".to_owned(),
                },
                EngineEvent::SubagentSpawned {
                    meta: event_meta("child", 1),
                    subagent_id: SubagentId("nested".to_owned()),
                    child_session_id: SessionId("grandchild".to_owned()),
                    task: "nested task".to_owned(),
                },
            ])
            .expect("child events");
        drop(child);
        drop(grandchild);
        let root = rw_store::session::EventEnvelope {
            schema_version: 1,
            sequence: SequenceId(0),
            event: EngineEvent::SubagentSpawned {
                meta: event_meta("root", 0),
                subagent_id: SubagentId("direct".to_owned()),
                child_session_id: SessionId("child".to_owned()),
                task: "direct task".to_owned(),
            },
        };

        let replay = historical_replay_items(storage.path(), "root", vec![root])
            .expect("derived historical replay");
        assert_eq!(replay.len(), 4);
        let HistoricalReplayItem::Progress { event, .. } = &replay[3] else {
            panic!("nested progress wrapper");
        };
        let EngineEvent::SubagentProgress { event, .. } = event else {
            panic!("direct progress wrapper");
        };
        assert_eq!(event["type"], "subagent_progress");
        assert_eq!(event["event"]["type"], "ui_notification");
        assert_eq!(event["event"]["title"], "grandchild");
    }

    #[test]
    fn historical_replay_charges_root_progress_wrapper_amplification() {
        let storage = tempfile::tempdir().expect("storage");
        let mut child = SessionEventLog::open(storage.path(), "child").expect("child log");
        child
            .append(EngineEvent::UiNotification {
                meta: event_meta("child", 0),
                plugin_id: "fixture".to_owned(),
                title: "child".to_owned(),
                message: "small event".to_owned(),
            })
            .expect("child event");
        drop(child);
        let root = rw_store::session::EventEnvelope {
            schema_version: 1,
            sequence: SequenceId(0),
            event: EngineEvent::SubagentSpawned {
                meta: event_meta("root", 0),
                subagent_id: SubagentId("x".repeat(MAX_REPLAY_PROGRESS_BYTES)),
                child_session_id: SessionId("child".to_owned()),
                task: "direct task".to_owned(),
            },
        };

        let error = historical_replay_items(storage.path(), "root", vec![root])
            .expect_err("amplified root wrapper must be bounded");
        assert!(error.to_string().contains("progress exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn historical_replay_rejects_symlinked_child_logs() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().expect("storage");
        let outside = tempfile::tempdir().expect("outside");
        let mut foreign =
            SessionEventLog::open(outside.path(), "foreign").expect("foreign child log");
        foreign
            .append(EngineEvent::UiNotification {
                meta: event_meta("foreign", 0),
                plugin_id: "fixture".to_owned(),
                title: "foreign".to_owned(),
                message: "must not load".to_owned(),
            })
            .expect("foreign event");
        fs::create_dir_all(storage.path().join("sessions")).expect("session root");
        symlink(
            outside.path().join("sessions/foreign"),
            storage.path().join("sessions/child"),
        )
        .expect("child symlink");
        let root = rw_store::session::EventEnvelope {
            schema_version: 1,
            sequence: SequenceId(0),
            event: EngineEvent::SubagentSpawned {
                meta: event_meta("root", 0),
                subagent_id: SubagentId("direct".to_owned()),
                child_session_id: SessionId("child".to_owned()),
                task: "direct task".to_owned(),
            },
        };

        let error = historical_replay_items(storage.path(), "root", vec![root])
            .expect_err("symlinked child must fail closed");
        assert!(error.to_string().contains("historical child session"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn historical_replay_process_gets_read_only_runtime_and_leaves_no_build_junk() {
        use std::os::unix::fs::PermissionsExt as _;

        let storage = tempfile::tempdir().expect("storage");
        let fixture = storage.path().join("fixture-tui");
        fs::write(storage.path().join("keybindings.toml"), "preset = 'vim'")
            .expect("user keybindings");
        fs::write(
            &fixture,
            b"#!/bin/sh\n\
              test \"$ROTTWEILER_REPLAY_MODE\" = \"1\" || exit 11\n\
              test \"$ROTTWEILER_SESSION_ID\" = \"history\" || exit 12\n\
              test -S \"$ROTTWEILER_ENGINE_SOCKET\" || exit 13\n\
              test -f \"$ROTTWEILER_ENGINE_TOKEN_FILE\" || exit 14\n\
              test \"$ROTTWEILER_TUI_KEYBINDINGS\" = \"preset = 'vim'\" || exit 15\n",
        )
        .expect("fixture script");
        fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700))
            .expect("fixture permissions");

        run_history_replay_with_tui(
            storage.path(),
            "history",
            engine()
                .events
                .iter()
                .filter_map(|item| match item {
                    HistoricalReplayItem::Durable(envelope) => Some(envelope.clone()),
                    HistoricalReplayItem::Progress { .. } => None,
                })
                .collect(),
            &fixture,
        )
        .await
        .expect("process replay");

        let mut runtime_entries = fs::read_dir(storage.path().join("run")).expect("runtime root");
        assert!(runtime_entries.next().is_none());
    }
}
