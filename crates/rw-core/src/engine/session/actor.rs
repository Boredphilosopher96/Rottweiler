use crate::engine::AgentLoopError;
use crate::engine::RoutedEvent;
use crate::engine::SessionRecoveredState;
use crate::engine::SessionUsage;
use crate::engine::dispatch::handle_actor_command;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::config::SessionActorConfig;
use crate::engine::session::handle::SessionHandle;
use crate::engine::session::recovery::interrupted_turn_recovery_events;
use crate::engine::session::recovery::recover_actor_from_journal;
use crate::engine::session::state::ActorCommand;
use crate::engine::session::state::ActorState;
use crate::engine::shutdown;
use crate::engine::turn::emit;
use crate::engine::turn::emit_batch;
use crate::engine::turn::handle_turn_signal;
use crate::engine::turn::hook_event_name;
use crate::engine::turn::start_turn;
use crate::engine::unavailable_cost;
use crate::engine::wire_turn_id;
use futures_util::FutureExt;
use rw_ext::HookEvent;
use rw_ext::HookFailurePolicy;
use rw_tools::ToolContext;
use rw_types::AccountingAttribution;
use rw_types::SessionId;
use rw_types::TurnAccounting;
use rw_types::hook_contract::HookInput;
use rw_types::hook_contract::HookSessionInput;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

/// Starts one single-writer session actor.
pub struct SessionActor;

impl SessionActor {
    /// Spawns the actor and returns its provider/UI-neutral handle.
    ///
    /// # Errors
    ///
    /// Rejects zero guardrails, empty aliases, or an unusable workspace root.
    pub fn spawn(mut config: SessionActorConfig) -> Result<SessionHandle, AgentLoopError> {
        if SessionId::validate(&config.session_id.0).is_err()
            || SessionId::validate(&config.budget_session_id.0).is_err()
        {
            return Err(AgentLoopError::InvalidConfiguration(
                "session and budget scope ids must satisfy the canonical session identifier grammar".to_owned(),
            ));
        }
        if config.model_alias.trim().is_empty() {
            return Err(AgentLoopError::InvalidConfiguration(
                "model alias must not be empty".to_owned(),
            ));
        }
        let recovered_mode = config
            .recovered
            .mode_id
            .as_ref()
            .map_or("execute", |mode| mode.0.as_str());
        if config.modes.get(recovered_mode).is_none() {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "recovered mode {recovered_mode:?} is not registered"
            )));
        }
        if config.recovered.permission_mode.is_some() {
            config
                .permissions
                .set_runtime_mode(config.recovered.permission_mode)
                .map_err(AgentLoopError::InvalidConfiguration)?;
        }
        if config.max_turns == 0
            || config.identical_tool_failure_limit == 0
            || config.max_output_tokens == 0
            || config.event_capacity == 0
        {
            return Err(AgentLoopError::InvalidConfiguration(
                "turn, doom-loop, output, and event limits must be greater than zero".to_owned(),
            ));
        }
        let tool_context = ToolContext::from_workspace_roots(
            std::iter::once(&config.workspace_root).chain(&config.additional_workspace_roots),
        )
        .map_err(|error| AgentLoopError::ToolContext(error.to_string()))?
        .with_session_id(config.session_id.clone())
        .with_mcp_tool_policy(config.tools.mcp_tool_policy().clone());
        let (command_tx, command_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(config.event_capacity);
        let active_turn = Arc::new(AtomicU64::new(0));
        let command_descriptors = Arc::new(RwLock::new(Arc::from(
            config.commands.descriptors().cloned().collect::<Vec<_>>(),
        )));
        let mode_registry = Arc::new(RwLock::new(Arc::clone(&config.modes)));
        let shutdown = shutdown::ActorShutdown::new(Arc::new(super::control::SessionControl::new(
            config.session_id.clone(),
            config.recovered.driver_client_id.clone(),
            Arc::clone(&config.event_clock),
        )));
        let handle = SessionHandle {
            shutdown: shutdown.clone(),
            commands: command_tx,
            events: event_tx.clone(),
            active_turn: active_turn.clone(),
            session_id: config.session_id.clone(),
            event_sink: Arc::clone(&config.event_sink),
            local_request_sequence: Arc::new(AtomicU64::new(0)),
            local_attached: Arc::new(AtomicBool::new(false)),
            local_last_seen: config.recovered.last_sequence,
            command_descriptors: Arc::clone(&command_descriptors),
            mode_registry: Arc::clone(&mode_registry),
            model: Arc::clone(&config.model),
        };
        // Startup input has one owner; route/workspace configuration clones must
        // not retain a second lifetime conversation after actor initialization.
        let recovered = std::mem::take(&mut config.recovered);
        let config = Arc::new(config);
        let retained = Arc::clone(&config);
        let task_shutdown = shutdown.clone();
        tokio::spawn(async move {
            if AssertUnwindSafe(run_actor(
                config,
                recovered,
                tool_context,
                command_rx,
                event_tx,
                shutdown::ActorControl {
                    active_turn,
                    command_descriptors: Arc::clone(&command_descriptors),
                    mode_registry,
                    shutdown: task_shutdown.clone(),
                },
            ))
            .catch_unwind()
            .await
            .is_err()
            {
                task_shutdown
                    .complete(Err("session actor exited without cleanup proof".to_owned()));
                shutdown::retain_unproven(retained).await;
            }
        });
        Ok(handle)
    }
}

pub(super) async fn dispatch_lifecycle_hook(
    event: HookEvent,
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
) -> bool {
    let input = HookSessionInput {
        session_id: config.session_id.0.clone(),
        workspace: config.workspace_root.to_string_lossy().into_owned(),
    };
    let input = match event {
        HookEvent::SessionStart => HookInput::SessionStart(input),
        HookEvent::SessionEnd => HookInput::SessionEnd(input),
        _ => unreachable!("lifecycle dispatcher accepts session events"),
    };
    let result = match config.hooks.dispatch(input).await {
        Ok(result) => result,
        Err(error) => {
            state.unsettled = Some(error.to_string());
            return false;
        }
    };
    for failure in result.failures() {
        if emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::HookFailure {
                event: hook_event_name(event).to_owned(),
                hook_id: failure.hook_id().to_owned(),
                fail_closed: failure.policy() == HookFailurePolicy::FailClosed,
                message: config.secret_redactor.redact(&failure.error().to_string()),
            },
        )
        .await
        .is_err()
        {
            return false;
        }
    }
    result.completed()
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_actor(
    config: Arc<SessionActorConfig>,
    recovered: SessionRecoveredState,
    mut tool_context: ToolContext,
    mut commands: mpsc::Receiver<ActorCommand>,
    events: broadcast::Sender<RoutedEvent>,
    control: shutdown::ActorControl,
) {
    let shutdown::ActorControl {
        active_turn,
        command_descriptors,
        mode_registry,
        shutdown,
    } = control;
    let interrupted_turn = recovered.interrupted_turn;
    let interrupted_compaction = recovered.interrupted_compaction;
    let recovery_events = interrupted_turn_recovery_events(&recovered);
    let mut state = ActorState::recover(
        config.session_id.clone(),
        Arc::clone(&config.event_clock),
        &config.model_alias,
        config.thinking,
        &config.modes,
        recovered,
        Arc::clone(&shutdown.control),
    );
    let mut config = config;
    let (turn_signals, mut signals) = mpsc::unbounded_channel();
    'startup: {
        if !config.startup_notifications.is_empty() {
            let startup_events = config.startup_notifications.iter().flat_map(|notice| {
                [
                    PendingEvent::PluginStatusChanged {
                        plugin_id: notice.plugin_id.clone(),
                        status: notice.status.clone(),
                    },
                    PendingEvent::UiNotification {
                        plugin_id: notice.plugin_id.clone(),
                        title: notice.title.clone(),
                        message: notice.message.clone(),
                    },
                ]
            });
            if emit_batch(
                &mut state,
                &events,
                &config.event_sink,
                startup_events.collect(),
            )
            .await
            .is_err()
            {
                state.unsettled = Some("session startup failed before completion".to_owned());
                break 'startup;
            }
        }
        if !dispatch_lifecycle_hook(HookEvent::SessionStart, &mut state, &config, &events).await {
            state.unsettled = Some("session startup failed before completion".to_owned());
            break 'startup;
        }
        if interrupted_compaction
            && emit(
                &mut state,
                &events,
                &config.event_sink,
                PendingEvent::Error {
                    message: "interrupted compaction was aborted during recovery".to_owned(),
                },
            )
            .await
            .is_err()
        {
            state.unsettled = Some("session startup failed before completion".to_owned());
            break 'startup;
        }
        if let Some(turn) = interrupted_turn {
            if emit_batch(&mut state, &events, &config.event_sink, recovery_events)
                .await
                .is_err()
            {
                state.unsettled = Some("session startup failed before completion".to_owned());
                break 'startup;
            }
            state.accounting.record(&TurnAccounting {
                turn_id: wire_turn_id(turn),
                attribution: AccountingAttribution::Main,
                usage: SessionUsage::default().into(),
                cost: unavailable_cost(),
            });
            state.completed_turns = state.completed_turns.saturating_add(1);
        }
        if !state.queued.is_empty() {
            state.queued_positions.clear();
            let messages = state
                .queued
                .drain(..)
                .map(|content| (content, Vec::new()))
                .collect();
            if start_turn(
                &mut state,
                &config,
                &tool_context,
                &turn_signals,
                &events,
                messages,
                &active_turn,
            )
            .await
            .is_err()
            {
                state.unsettled = Some("session startup failed before completion".to_owned());
                break 'startup;
            }
        }
    }
    let mut commands_open = true;
    let mut closing_started = None;
    let mut cleanup = None;
    loop {
        if shutdown.requested() || !commands_open || state.unsettled.is_some() {
            state.control.close();
            state.closing = true;
            state.tasks.cancel();
            if let Some(running) = &state.running {
                running.cancellation.cancel();
            }
            closing_started.get_or_insert_with(tokio::time::Instant::now);
        }
        if let Some(error) = state.tasks.failure() {
            state.unsettled.get_or_insert_with(|| error.to_string());
        }
        if state.closing
            && state.tasks.idle()
            && state.pending_command.is_none()
            && state.pending_model_preparation.is_none()
            && signals.is_empty()
            && cleanup.is_none()
        {
            cleanup = Some(shutdown::start_cleanup(
                Arc::clone(&config),
                turn_signals.clone(),
                state.unsettled.clone(),
            ));
        }
        if !state.closing
            && state.running.is_none()
            && !state.initialization_running
            && state.active_shell.is_none()
            && state.pending_command.is_none()
            && state.pending_model_preparation.is_none()
            && !state.queued.is_empty()
        {
            state.queued_positions.clear();
            let queued = state
                .queued
                .drain(..)
                .map(|content| (content, Vec::new()))
                .collect();
            if let Err(error) = start_turn(
                &mut state,
                &config,
                &tool_context,
                &turn_signals,
                &events,
                queued,
                &active_turn,
            )
            .await
            {
                state.unsettled.get_or_insert_with(|| error.to_string());
                continue;
            }
        }
        let tasks = state.tasks.clone();
        tokio::select! {
            result = crate::engine::dispatch::model_job::wait(&mut state.pending_model_preparation) => {
                crate::engine::dispatch::model_job::finish(result, crate::engine::dispatch::DispatchContext {
                    state: &mut state, config: &mut config, tool_context: &mut tool_context,
                    turn_signals: &turn_signals, events: &events, active_turn: &active_turn,
                    command_descriptors: &command_descriptors, mode_registry: &mode_registry,
                }).await;
            }
            result = crate::engine::dispatch::command_job::wait(&mut state.pending_command) => {
                crate::engine::dispatch::command_job::finish(result, crate::engine::dispatch::DispatchContext {
                    state: &mut state, config: &mut config, tool_context: &mut tool_context,
                    turn_signals: &turn_signals, events: &events, active_turn: &active_turn,
                    command_descriptors: &command_descriptors, mode_registry: &mode_registry,
                }).await;
            }
            () = shutdown.cancelled(), if !state.closing => {},
            () = tasks.changed() => {},
            () = shutdown::deadline(closing_started), if closing_started.is_some() => {
                state.unsettled.get_or_insert_with(|| "session shutdown deadline expired before effect settlement".to_owned());
                break;
            }
            result = shutdown::cleanup_result(&mut cleanup), if cleanup.is_some() => {
                if let Err(error) = result { state.unsettled.get_or_insert(error); }
                break;
            }
            command = commands.recv(), if commands_open => {
                let Some(command) = command else { commands_open = false; continue; };
                let command = if state.closing {
                    let Some(command) = shutdown::admit_internal(command) else { continue; };
                    command
                } else { command };
                handle_actor_command(
                    command, &mut state, &mut config, &mut tool_context, &turn_signals,
                    &events, &active_turn, &command_descriptors, &mode_registry,
                ).await;
            }
            signal = signals.recv() => {
                let Some(signal) = signal else {
                    state.unsettled.get_or_insert_with(|| "session effect signal channel closed".to_owned());
                    break;
                };
                if let Err(error) = handle_turn_signal(
                    signal, &mut state, &config, &turn_signals, &events, &active_turn,
                ).await {
                    if state.closing {
                        state.unsettled.get_or_insert_with(|| error.to_string());
                    } else {
                        while signals.try_recv().is_ok() {}
                        if let Err(error) = recover_actor_from_journal(&mut state, &config, &events, &active_turn).await {
                            state.unsettled.get_or_insert_with(|| error.to_string());
                        }
                    }
                }
            }
        }
    }
    active_turn.store(0, Ordering::Release);
    if let Some(message) = state.unsettled.clone() {
        shutdown.complete(Err(message));
        shutdown::retain_unproven((state, config, cleanup, commands, signals)).await;
    } else {
        shutdown.complete(Ok(()));
    }
}
