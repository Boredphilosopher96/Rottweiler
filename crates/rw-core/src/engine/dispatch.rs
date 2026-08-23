#[allow(clippy::wildcard_imports)]
use super::*;

fn protocol_rejection(code: &str, message: impl Into<String>) -> CommandOutcome {
    CommandOutcome::Rejected {
        error: EngineError {
            category: EngineErrorCategory::Protocol,
            code: code.to_owned(),
            message: message.into(),
            retryable: false,
            details: None,
        },
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn prepare_user_message(
    content: &str,
    attachments: &[Attachment],
    model_alias: &str,
    model: &dyn ModelDriver,
) -> Result<PreparedUserMessage, String> {
    if attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(format!(
            "at most {MAX_ATTACHMENTS_PER_MESSAGE} attachments are allowed"
        ));
    }
    let mut total_bytes = 0_usize;
    let mut stored_attachments = Vec::with_capacity(attachments.len());
    let mut attachment_blocks = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        if attachment.name.is_empty()
            || attachment.name.len() > 255
            || attachment.name == "."
            || attachment.name == ".."
            || attachment
                .name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err("attachment names must be safe single path components".to_owned());
        }
        if let Some(source_path) = attachment.source_path.as_deref()
            && !is_safe_relative_attachment_path(source_path)
        {
            return Err(
                "attachment source paths must be normalized workspace-relative paths".to_owned(),
            );
        }
        if attachment.media_type.trim() != attachment.media_type
            || attachment.media_type.to_ascii_lowercase() != attachment.media_type
        {
            return Err(
                "attachment media types must be canonical lowercase MIME values".to_owned(),
            );
        }
        let (byte_len, content_hash, block) = match (
            &attachment.data,
            attachment.media_type.as_str(),
        ) {
            (AttachmentData::Text { content }, media_type)
                if media_type.starts_with("text/") || media_type == "application/json" =>
            {
                if content.len() > MAX_TEXT_ATTACHMENT_BYTES {
                    return Err(format!(
                        "text attachment {:?} exceeds {MAX_TEXT_ATTACHMENT_BYTES} bytes",
                        attachment.name
                    ));
                }
                let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
                let label = attachment
                    .source_path
                    .as_deref()
                    .unwrap_or(&attachment.name);
                let text = format!("Attached file {label:?} ({media_type}):\n{content}");
                (content.len(), hash, Block::Text { text })
            }
            (AttachmentData::InlineBase64 { data }, media_type)
                if matches!(
                    media_type,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) =>
            {
                if !model.supports_vision(model_alias) {
                    return Err(format!(
                        "model alias {model_alias:?} does not support image attachments"
                    ));
                }
                let decoded_len = canonical_base64_decoded_len(data).ok_or_else(|| {
                    format!(
                        "image attachment {:?} is not canonical base64",
                        attachment.name
                    )
                })?;
                if decoded_len > MAX_IMAGE_ATTACHMENT_BYTES {
                    return Err(format!(
                        "image attachment {:?} exceeds {MAX_IMAGE_ATTACHMENT_BYTES} decoded bytes",
                        attachment.name
                    ));
                }
                let hash = blake3::hash(data.as_bytes()).to_hex().to_string();
                (
                    decoded_len,
                    hash,
                    Block::Image {
                        media_type: media_type.to_owned(),
                        data: ImageRef::InlineBase64 { data: data.clone() },
                    },
                )
            }
            _ => {
                return Err(format!(
                    "attachment {:?} has unsupported data for media type {:?}",
                    attachment.name, attachment.media_type
                ));
            }
        };
        total_bytes = total_bytes.saturating_add(byte_len);
        if total_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err(format!(
                "attachments exceed the {MAX_TOTAL_ATTACHMENT_BYTES}-byte total limit"
            ));
        }
        stored_attachments.push(StoredAttachment {
            name: attachment.name.clone(),
            source_path: attachment.source_path.clone(),
            media_type: attachment.media_type.clone(),
            content_hash,
            byte_len: u64::try_from(byte_len).unwrap_or(u64::MAX),
        });
        attachment_blocks.push(block);
    }
    if content.is_empty() && attachment_blocks.is_empty() {
        return Err("message content and attachments cannot both be empty".to_owned());
    }
    Ok(PreparedUserMessage {
        content: content.to_owned(),
        stored_attachments,
        attachment_blocks,
    })
}

fn is_safe_relative_attachment_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4_096
        && !value.chars().any(char::is_control)
        && !value.contains('\\')
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn canonical_base64_decoded_len(data: &str) -> Option<usize> {
    if data.is_empty() || !data.len().is_multiple_of(4) || !data.is_ascii() {
        return None;
    }
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    if padding > 2 {
        return None;
    }
    let payload_len = data.len().checked_sub(padding)?;
    if data
        .bytes()
        .take(payload_len)
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')))
        || data.bytes().skip(payload_len).any(|byte| byte != b'=')
    {
        return None;
    }
    data.len()
        .checked_div(4)?
        .checked_mul(3)?
        .checked_sub(padding)
}

fn send_ack(
    state: &ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    meta: &CommandMeta,
    session_id: Option<SessionId>,
    outcome: CommandOutcome,
) {
    let _ = events.send(RoutedEvent {
        target: Some(meta.client_id.clone()),
        event: EngineEvent::CommandAcknowledged {
            meta: CommandAckMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: meta.client_id.clone(),
                request_id: meta.request_id.clone(),
                emitted_at: state.event_clock.emitted_at(),
            },
            session_id,
            outcome,
        },
    });
}

fn send_connection_event(
    events: &broadcast::Sender<RoutedEvent>,
    client_id: &ClientId,
    event: EngineEvent,
) {
    let _ = events.send(RoutedEvent {
        target: Some(client_id.clone()),
        event,
    });
}

fn query_meta(state: &ActorState, meta: &CommandMeta) -> CommandAckMeta {
    CommandAckMeta {
        protocol_version: PROTOCOL_VERSION,
        client_id: meta.client_id.clone(),
        request_id: meta.request_id.clone(),
        emitted_at: state.event_clock.emitted_at(),
    }
}

async fn apply_context_surgery(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    sink: &Arc<dyn SessionEventSink>,
    item_id: ContextItemId,
    pinned: bool,
) -> Result<(), AgentLoopError> {
    let effective_after_agent_turn = state.next_turn;
    let pending = if pinned {
        PendingEvent::ContextItemPinned {
            item_id: item_id.clone(),
            effective_after_agent_turn,
        }
    } else {
        PendingEvent::ContextItemEvicted {
            item_id: item_id.clone(),
            effective_after_agent_turn,
        }
    };
    emit(state, events, sink, pending).await?;
    state.context_surgery.push(ContextSurgeryAction {
        item_id,
        pinned,
        effective_after_agent_turn,
    });
    Ok(())
}

async fn apply_registered_context_surgery(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
    item_id: ContextItemId,
    pinned: bool,
) -> Result<(), AgentLoopError> {
    if !item_id.0.starts_with("conversation:") {
        return Err(AgentLoopError::InvalidConfiguration(
            "protected_context_item: only conversation-resident context items support pin or eviction"
                .to_owned(),
        ));
    }
    let known = assemble_session_context(
        config,
        &state.conversation,
        &state.queued,
        &state.context_surgery,
        &state.pruned_tool_outputs,
        false,
    )
    .is_ok_and(|assembled| assembled.items.iter().any(|item| item.id.0 == item_id.0));
    if !known {
        return Err(AgentLoopError::InvalidConfiguration(
            "unknown_context_item: context item is not present in the current inventory".to_owned(),
        ));
    }
    apply_context_surgery(state, events, &config.event_sink, item_id, pinned).await
}

fn requires_driver(command: &ClientCommand) -> bool {
    !matches!(
        command,
        ClientCommand::CreateSession { .. }
            | ClientCommand::AttachSession { .. }
            | ClientCommand::TakeDriver { .. }
            | ClientCommand::GetContext { .. }
            | ClientCommand::GetCost { .. }
            | ClientCommand::GetSessionReview { .. }
            | ClientCommand::DumpPrompt { .. }
            | ClientCommand::ListPermissions { .. }
            | ClientCommand::RenameSession { .. }
            | ClientCommand::AttachDevelopmentPlugin { .. }
            | ClientCommand::DetachDevelopmentPlugin { .. }
    )
}

const fn permission_mode_descriptor(
    mode: rw_types::PermissionModeDescriptor,
) -> PermissionModeDescriptor {
    mode
}

fn permission_rule_id(scope: &str, rule: &PermissionRule) -> String {
    let mut digest = blake3::Hasher::new();
    digest.update(b"rottweiler-permission-rule-row-v1\0");
    digest.update(scope.as_bytes());
    digest.update(b"\0");
    digest.update(format!("{:?}", rule.action).as_bytes());
    digest.update(b"\0");
    digest.update(rule.pattern.as_bytes());
    format!("{scope}:{}", &digest.finalize().to_hex()[..24])
}

fn bounded_permission_rule(scope: &str, rule: &PermissionRule) -> Option<PermissionRuleDescriptor> {
    (rule.pattern.len() <= MAX_PERMISSION_PATTERN_BYTES
        && !rule.pattern.chars().any(char::is_control))
    .then(|| PermissionRuleDescriptor {
        id: permission_rule_id(scope, rule),
        pattern: rule.pattern.clone(),
        action: rule.action,
    })
}

pub(super) fn permission_state(permissions: &PermissionGate) -> PermissionStateDescriptor {
    let snapshot = permissions.snapshot();
    let mut truncated = false;
    let mut collect_rules = |scope: &str, rules: &[PermissionRule]| {
        let mut rows = Vec::new();
        for rule in rules {
            if rows.len() >= MAX_PERMISSION_RULES_PER_SCOPE {
                truncated = true;
                break;
            }
            if let Some(row) = bounded_permission_rule(scope, rule) {
                rows.push(row);
            } else {
                truncated = true;
            }
        }
        rows
    };
    let effective_rules = collect_rules("effective", &snapshot.rules);
    let session_rules = collect_rules("session", &snapshot.session_rules);
    let remembered = permissions.approval_snapshot();
    let mut approvals = Vec::new();
    for (scope, rows) in [
        (PermissionApprovalScope::Session, remembered.session),
        (PermissionApprovalScope::Project, remembered.project),
    ] {
        for approval in rows {
            if approvals.len() >= MAX_PERMISSION_APPROVALS {
                truncated = true;
                break;
            }
            if approval.id.len() > MAX_PERMISSION_ID_BYTES
                || approval.tool_name.len() > MAX_PERMISSION_LABEL_BYTES
                || approval.canonical_summary.len() > MAX_PERMISSION_LABEL_BYTES
                || approval.id.chars().any(char::is_control)
                || approval.tool_name.chars().any(char::is_control)
                || approval.canonical_summary.chars().any(char::is_control)
            {
                truncated = true;
                continue;
            }
            approvals.push(PermissionApprovalDescriptor {
                id: approval.id,
                scope,
                tool_name: approval.tool_name,
                summary: approval.canonical_summary,
            });
        }
    }
    PermissionStateDescriptor {
        default: snapshot.default,
        runtime_mode: snapshot.runtime_mode.map(permission_mode_descriptor),
        effective_rules,
        // Project configuration cannot grant permission authority. Remembered
        // project approvals are represented separately above.
        project_rules: Vec::new(),
        session_rules,
        approvals,
        truncated,
    }
}

fn apply_permission_command(
    command: &ClientCommand,
    permissions: &PermissionGate,
) -> Result<PermissionStateDescriptor, String> {
    match command {
        ClientCommand::ListPermissions { .. } => {}
        ClientCommand::AddSessionPermissionRule {
            pattern, action, ..
        } => {
            if pattern.is_empty()
                || pattern.len() > MAX_PERMISSION_PATTERN_BYTES
                || pattern.chars().any(char::is_control)
            {
                return Err("permission rule is empty or exceeds its safety limit".to_owned());
            }
            permissions.add_session_rule(PermissionRule {
                pattern: pattern.clone(),
                action: *action,
            })?;
        }
        ClientCommand::RemoveSessionPermissionRule { rule_id, .. } => {
            if rule_id.is_empty() || rule_id.len() > MAX_PERMISSION_ID_BYTES {
                return Err("permission rule id is invalid".to_owned());
            }
            let snapshot = permissions.snapshot();
            let pattern = snapshot
                .session_rules
                .iter()
                .find(|rule| permission_rule_id("session", rule) == *rule_id)
                .map(|rule| rule.pattern.clone())
                .ok_or_else(|| "permission rule is no longer present".to_owned())?;
            if !permissions.remove_session_rule(&pattern) {
                return Err("permission rule is no longer present".to_owned());
            }
        }
        ClientCommand::RevokePermissionApproval {
            approval_id, scope, ..
        } => {
            if approval_id.is_empty() || approval_id.len() > MAX_PERMISSION_ID_BYTES {
                return Err("permission approval id is invalid".to_owned());
            }
            let approvals = permissions.approval_snapshot();
            let known = match scope {
                PermissionApprovalScope::Session => approvals.session,
                PermissionApprovalScope::Project => approvals.project,
            }
            .iter()
            .any(|approval| approval.id == *approval_id);
            if !known {
                return Err("permission approval is no longer present".to_owned());
            }
            let removed = match scope {
                PermissionApprovalScope::Session => {
                    permissions.revoke_session_approvals(Some(approval_id))
                }
                PermissionApprovalScope::Project => permissions
                    .revoke_project_approvals(Some(approval_id))
                    .map_err(|_| "project approval revocation failed".to_owned())?,
            };
            if removed != 1 {
                return Err("permission approval is no longer present".to_owned());
            }
        }
        _ => return Err("command is not a permission-management operation".to_owned()),
    }
    Ok(permission_state(permissions))
}

fn unsupported_in_m2(command: &ClientCommand) -> bool {
    matches!(
        command,
        ClientCommand::CreateSession { .. }
            | ClientCommand::ResumeSession { .. }
            | ClientCommand::Fork { .. }
            | ClientCommand::ListSessions { .. }
            | ClientCommand::ListCommands { .. }
            | ClientCommand::ListModes { .. }
            | ClientCommand::ListModels { .. }
            | ClientCommand::SearchWorkspaceFiles { .. }
            | ClientCommand::PreviewWorkspaceFile { .. }
            | ClientCommand::GetWorkspaceStatus { .. }
            | ClientCommand::ShutdownHost { .. }
    )
}

pub(super) async fn commit_prepared_model_switch(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    prepared: PreparedModelSwitch,
    clear_context: bool,
) -> Result<(), AgentLoopError> {
    let mut durable = Vec::with_capacity(if clear_context { 2 } else { 1 });
    if clear_context {
        durable.push(PendingEvent::ModelContextCleared {
            strategy: ModelContextTransfer::StartWithoutContext,
        });
    }
    durable.push(PendingEvent::ModelChanged {
        model: prepared.model.clone(),
        provider: prepared.provider.clone(),
        thinking: prepared.thinking,
    });
    let result = emit_batch(state, events, &config.event_sink, durable).await;
    if result.is_ok() {
        if clear_context {
            state.conversation.retain(|turn| turn.role == Role::System);
            state.context_surgery.clear();
            state.pruned_tool_outputs.clear();
        }
        config.model.commit_prepared_model(&prepared.model.0);
        state.model_alias = prepared.model.0;
        state.provider = prepared.provider;
        state.thinking = prepared.thinking;
    } else {
        config.model.discard_prepared_model(&prepared.model.0);
    }
    result
}

fn start_manual_compaction(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    turn_signals: &mpsc::UnboundedSender<TurnSignal>,
    active_turn: &Arc<AtomicU64>,
    instructions: Option<String>,
    model_switch: Option<PreparedModelSwitch>,
    completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
) {
    let summary_turn = state.next_turn;
    let cancellation = CancellationToken::default();
    state.running = Some(RunningTurn {
        id: summary_turn,
        cancellation: cancellation.clone(),
        caused_by: state.transient_cause.clone(),
    });
    active_turn.store(summary_turn, Ordering::Release);
    let mut conversation = state.conversation.clone();
    let mut context_surgery = state.context_surgery.clone();
    let local_session_accounting = session_accounting_fallback(&state.accounting);
    let config = Arc::new(config.with_model_route_and_mode(
        state.model_alias.clone(),
        state.provider.clone(),
        &state.mode_id,
    ));
    let signals = turn_signals.clone();
    tokio::spawn(async move {
        let result = async {
            let pre_budget = evaluate_budget(
                summary_turn,
                config.event_clock.as_ref(),
                &config.event_sink,
                &config.model.budget_config(),
                local_session_accounting,
                0,
                0,
            )
            .await?;
            for event in pre_budget.events {
                persist_event(&signals, event).await?;
            }
            if pre_budget.hard_stop {
                return Err(AgentLoopError::InvalidConfiguration(
                    "budget hard cap prevents compaction model call".to_owned(),
                ));
            }
            compact_during_turn(
                summary_turn,
                &mut conversation,
                &mut context_surgery,
                CompactionReason::Manual,
                &config,
                &cancellation,
                &signals,
                local_session_accounting,
                0,
                0,
                instructions,
            )
            .await
            .map(|_| ())
        }
        .await;
        if let Err(error) = &result {
            let _ = persist_event(
                &signals,
                PendingEvent::Error {
                    message: error.to_string(),
                },
            )
            .await;
        }
        let _ = signals.send(TurnSignal::ManualCompactionComplete {
            turn: summary_turn,
            conversation,
            context_surgery,
            result,
            model_switch,
            completion,
        });
    });
}

fn start_workspace_initialization(
    workspace: PathBuf,
    depth: InitDepth,
    session_id: SessionId,
    mutation_turn: u64,
    call_id: String,
    checkpoints: Arc<dyn MutationCheckpointCoordinator>,
    signals: mpsc::UnboundedSender<TurnSignal>,
) {
    let name = match depth {
        InitDepth::Root => "init",
        InitDepth::Deep => "deep-init",
    };
    tokio::spawn(async move {
        let result = async {
            let plan = tokio::task::spawn_blocking(move || {
                plan_init(&workspace, depth, crate::DEFAULT_INIT_FILE_BUDGET_BYTES)
            })
            .await
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
            let scope = MutationScope::Paths(plan.files().keys().cloned().collect());
            validate_mutation_scope(&scope)?;
            let checkpoint = checkpoints
                .begin(&session_id, mutation_turn, &call_id, &scope)
                .await?;
            let applied = tokio::task::spawn_blocking(move || apply_init_plan(&plan)).await;
            let applied = match applied {
                Ok(result) => {
                    result.map_err(|error| AgentLoopError::Persistence(error.to_string()))
                }
                Err(error) => Err(AgentLoopError::Persistence(error.to_string())),
            };
            let outcome = if applied.is_ok() {
                MutationCheckpointOutcome::Completed
            } else {
                MutationCheckpointOutcome::Failed
            };
            checkpoints.finish(&checkpoint, outcome).await?;
            let created = applied?;
            let generated = created
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!(
                "generated {} instruction file(s): {generated}",
                created.len()
            ))
        }
        .await;
        let _ = signals.send(TurnSignal::InitializationComplete { name, result });
    });
}

async fn handle_plugin_message(
    plugin_id: String,
    content: String,
    state: &mut ActorState,
    runtime: StartTurnRuntime<'_>,
) -> Result<MessageDisposition, AgentLoopError> {
    validate_plugin_id(&plugin_id)?;
    validate_plugin_text("injected message", &content, MAX_PLUGIN_MESSAGE_BYTES)?;
    if state.poisoned {
        return Err(AgentLoopError::InvalidConfiguration(
            "session requires recovery before plugin message injection".to_owned(),
        ));
    }
    if state.active_shell.is_some() {
        return Err(AgentLoopError::InvalidConfiguration(
            "an agent turn cannot start while the foreground user shell is active".to_owned(),
        ));
    }
    if state.initialization_running {
        return Err(AgentLoopError::InvalidConfiguration(
            "workspace initialization is still running".to_owned(),
        ));
    }
    let content = runtime.config.secret_redactor.redact(&content);
    validate_plugin_text(
        "redacted injected message",
        &content,
        MAX_PLUGIN_MESSAGE_BYTES,
    )?;
    let disposition = if state.running.is_some() {
        let position = state
            .queued_positions
            .back()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                AgentLoopError::InvalidConfiguration(
                    "queued message position space is exhausted".to_owned(),
                )
            })?;
        state.queued.push_back(content.clone());
        state.queued_positions.push_back(position);
        if let Err(error) = emit(
            state,
            runtime.events,
            &runtime.config.event_sink,
            PendingEvent::MessageQueued {
                position,
                content: content.clone(),
                attachments: Vec::new(),
            },
        )
        .await
        {
            state.queued.pop_back();
            state.queued_positions.pop_back();
            return Err(error);
        }
        MessageDisposition::Queued
    } else {
        start_turn(
            state,
            runtime.config,
            runtime.tool_context,
            runtime.signals,
            runtime.events,
            vec![(content.clone(), Vec::new())],
            runtime.active_turn,
        )
        .await?;
        MessageDisposition::Started
    };
    if let Err(error) = emit(
        state,
        runtime.events,
        &runtime.config.event_sink,
        PendingEvent::PluginMessageInjected {
            plugin_id,
            content,
            queued: disposition == MessageDisposition::Queued,
        },
    )
    .await
    {
        if let Some(running) = &state.running {
            running.cancellation.cancel();
        }
        state.poisoned = true;
        return Err(error);
    }
    Ok(disposition)
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_actor_command(
    command: ActorCommand,
    state: &mut ActorState,
    config: &mut Arc<SessionActorConfig>,
    tool_context: &mut ToolContext,
    turn_signals: &mpsc::UnboundedSender<TurnSignal>,
    events: &broadcast::Sender<RoutedEvent>,
    active_turn: &Arc<AtomicU64>,
    command_descriptors: &Arc<RwLock<Arc<[CommandDescriptor]>>>,
    mode_registry: &Arc<RwLock<Arc<ModeRegistry>>>,
) {
    match command {
        ActorCommand::Protocol {
            mut command,
            respond,
            mut completion,
        } => {
            let meta = command.meta().clone();
            let session = command.session_id().cloned();
            let rejection = if meta.protocol_version != PROTOCOL_VERSION {
                Some(protocol_rejection(
                    "unsupported_protocol_version",
                    format!(
                        "protocol version {} is unsupported; expected {PROTOCOL_VERSION}",
                        meta.protocol_version
                    ),
                ))
            } else if session.as_ref().is_some_and(|id| id != &config.session_id) {
                Some(protocol_rejection(
                    "session_mismatch",
                    "command session id does not match this actor",
                ))
            } else if unsupported_in_m2(&command) {
                Some(protocol_rejection(
                    "command_not_available",
                    "command is not available in milestone M2",
                ))
            } else if state.poisoned
                && !matches!(
                    (&command, &state.pending_rewind),
                    (
                        ClientCommand::Rewind {
                            target: RewindTarget::Turn { turn_id },
                            ..
                        },
                        Some((pending_turn, _))
                    ) if turn_id.0 == pending_turn.to_string()
                )
            {
                Some(protocol_rejection(
                    "session_requires_recovery",
                    "session is fail-closed until checkpoint journal recovery completes",
                ))
            } else if requires_driver(&command)
                && state.driver_client_id.as_ref() != Some(&meta.client_id)
            {
                Some(protocol_rejection(
                    "driver_required",
                    "mutating commands are accepted only from the current driver",
                ))
            } else {
                None
            };
            if let Some(outcome) = rejection {
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return;
            }

            if let ClientCommand::UserShellEnded {
                captured_output, ..
            } = &mut command
            {
                *captured_output = captured_output
                    .take()
                    .map(|output| config.secret_redactor.redact(&output));
            }
            if let ClientCommand::RenameSession { title, .. } = &mut command {
                let Some(normalized) = normalize_manual_session_title(title) else {
                    let outcome = protocol_rejection(
                        "invalid_session_title",
                        format!(
                            "session title must be non-empty, contain no control characters, and contain at most {SESSION_TITLE_MAX_CHARS} characters"
                        ),
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                };
                *title = normalized;
            }

            match &command {
                ClientCommand::AttachSession { role, .. } => {
                    if *role == ClientRole::Driver
                        && state
                            .driver_client_id
                            .as_ref()
                            .is_some_and(|driver| driver != &meta.client_id)
                    {
                        let outcome = protocol_rejection(
                            "driver_lease_held",
                            "another client holds the driver lease; attach as observer or take it explicitly",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
                ClientCommand::SendMessage { .. } if state.active_shell.is_some() => {
                    let outcome = protocol_rejection(
                        "user_shell_active",
                        "an agent turn cannot start while the foreground user shell is active",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::AttachDevelopmentPlugin { source, .. }
                    if state.running.is_some()
                        || state.active_shell.is_some()
                        || state.driver_client_id.is_none()
                        || source.is_empty()
                        || source.len() > 4096
                        || source.chars().any(char::is_control) =>
                {
                    let outcome = protocol_rejection(
                        "development_attach_requires_idle_session",
                        "development plugin attachment requires an idle session and one bounded source path",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::DetachDevelopmentPlugin { .. }
                    if state.running.is_some()
                        || state.active_shell.is_some()
                        || state.driver_client_id.is_none() =>
                {
                    let outcome = protocol_rejection(
                        "development_detach_requires_idle_session",
                        "development plugin detachment requires an idle session",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SendMessage { attachments, .. }
                    if state.running.is_some() && !attachments.is_empty() =>
                {
                    let outcome = protocol_rejection(
                        "attachment_queue_unsupported",
                        "messages with attachments require an idle session so their provider-neutral blocks commit atomically",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SendMessage {
                    content,
                    attachments,
                    ..
                } if content.trim_start().starts_with('/') && !attachments.is_empty() => {
                    let outcome = protocol_rejection(
                        "command_attachments_unsupported",
                        "slash commands do not accept message attachments",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SendMessage {
                    content,
                    attachments,
                    ..
                } => {
                    if attachments.iter().any(|attachment| {
                        matches!(&attachment.data, AttachmentData::InlineBase64 { .. })
                            && matches!(
                                attachment.media_type.as_str(),
                                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                            )
                    }) && let Err(error) = config.model.prepare_model(&state.model_alias).await
                    {
                        let outcome = protocol_rejection(
                            "model_unavailable",
                            format!(
                                "the selected model could not be prepared for image attachments: {error}"
                            ),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                    if let Err(message) = prepare_user_message(
                        content,
                        attachments,
                        &state.model_alias,
                        config.model.as_ref(),
                    ) {
                        let outcome = protocol_rejection("invalid_attachment", message);
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
                ClientCommand::SwitchModel { .. }
                | ClientCommand::SwitchMode { .. }
                | ClientCommand::ApprovePlan { .. }
                    if state.running.is_some() || state.active_shell.is_some() =>
                {
                    let outcome = protocol_rejection(
                        "session_not_idle",
                        "model switching requires an idle session with no active user shell",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SwitchModel { .. } if !state.pending_model_switches.is_empty() => {
                    let outcome = protocol_rejection(
                        "model_switch_pending",
                        "choose how to transfer context for the pending model switch first",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SwitchModel {
                    model, provider, ..
                } => {
                    if !config.model.has_model_alias(&model.0) {
                        let outcome = protocol_rejection(
                            "unknown_model_alias",
                            format!("model {:?} is unavailable", model.0),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                    if let Some(provider) = provider
                        && !config.model.has_provider_for_alias(&model.0, provider)
                    {
                        let outcome = protocol_rejection(
                            "unknown_provider_route",
                            format!(
                                "model alias {:?} has no configured route through provider {:?}",
                                model.0, provider
                            ),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                    let has_prior_context = state
                        .conversation
                        .iter()
                        .any(|turn| turn.role != Role::System);
                    let requires_context_choice = has_prior_context
                        && (state.model_alias != model.0
                            || state.provider.as_ref() != provider.as_ref());
                    if !requires_context_choice
                        && let Err(error) = config.model.prepare_model(&model.0).await
                    {
                        let outcome = protocol_rejection("unknown_model_alias", error.to_string());
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
                ClientCommand::SwitchMode { mode, .. } if config.modes.get(&mode.0).is_none() => {
                    let outcome = protocol_rejection(
                        "unknown_mode",
                        format!("mode {:?} is not registered", mode.0),
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SwitchMode { mode, .. }
                    if config.modes.get(&mode.0).is_some_and(|definition| {
                        mode_permission_base(definition) == SessionMode::Execute
                    }) && state.plan_gate_active =>
                {
                    let outcome = protocol_rejection(
                        "plan_approval_required",
                        "Plan mode can enter Execute only after the submitted plan is approved",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::ApprovePlan { .. } if state.pending_plan.is_none() => {
                    let outcome = protocol_rejection(
                        "no_pending_plan",
                        "there is no submitted plan awaiting review",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::UserShellStarted { command, .. }
                    if command.trim().is_empty()
                        || state.running.is_some()
                        || state.active_shell.is_some()
                        || config.tools.session_activity(&state.session_id).is_some() =>
                {
                    let outcome = protocol_rejection(
                        "shell_start_rejected",
                        "a non-empty foreground shell may start only while the session is idle",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::UserShellEnded {
                    shell_id,
                    captured_output,
                    ..
                } if state.active_shell.as_ref().map(|shell| &shell.shell_id) != Some(shell_id)
                    || captured_output
                        .as_ref()
                        .is_some_and(|output| output.len() > MAX_CAPTURED_SHELL_OUTPUT_BYTES) =>
                {
                    let outcome = protocol_rejection(
                        "shell_end_rejected",
                        "shell end must match the active shell id and its captured output must fit the durable limit",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::Rewind {
                    target: RewindTarget::Checkpoint { .. },
                    ..
                } => {
                    let outcome = protocol_rejection(
                        "checkpoint_target_not_available",
                        "rewind by checkpoint id is not available in milestone M2",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::Rewind {
                    target: RewindTarget::Turn { turn_id },
                    ..
                } if parse_turn_id(turn_id).is_err() => {
                    let outcome = protocol_rejection(
                        "invalid_turn_id",
                        "rewind turn id must be an unsigned decimal integer",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::Rewind {
                    target: RewindTarget::Turn { turn_id },
                    ..
                } if state.running.is_some()
                    || config.tools.session_activity(&state.session_id).is_some()
                    || parse_turn_id(turn_id).is_ok_and(|to_turn| {
                        !state.turn_ends.contains_key(&to_turn)
                            && state.pending_rewind.as_ref().map(|pending| pending.0)
                                != Some(to_turn)
                    }) =>
                {
                    let outcome = protocol_rejection(
                        "invalid_rewind_target",
                        "rewind requires an idle session and a completed turn target",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::GetSessionReview { .. }
                    if state.running.is_some()
                        || state.active_shell.is_some()
                        || config.tools.session_activity(&state.session_id).is_some() =>
                {
                    let outcome = protocol_rejection(
                        "session_not_idle",
                        "session review requires an idle session",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::ReviewFile {
                    path, current_hash, ..
                } if state.running.is_some()
                    || state.active_shell.is_some()
                    || config.tools.session_activity(&state.session_id).is_some()
                    || !review_path_is_valid(path)
                    || !review_hash_is_valid(current_hash) =>
                {
                    let outcome = protocol_rejection(
                        "invalid_review_file",
                        "review decisions require an idle session, a safe relative path, and the displayed current hash",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::ApproveTool { tool_call_id, .. }
                    if !state.pending_approvals.contains_key(&tool_call_id.0) =>
                {
                    let outcome =
                        protocol_rejection("unknown_approval", "tool approval is not pending");
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::ApproveTool {
                    tool_call_id,
                    binding,
                    ..
                } if state
                    .pending_approvals
                    .get(&tool_call_id.0)
                    .is_some_and(|pending| pending.binding.as_ref() != binding.as_ref()) =>
                {
                    let outcome = protocol_rejection(
                        "approval_binding_mismatch",
                        "approval binding does not match the displayed proposal; the approval remains pending",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::ApproveTool {
                    tool_call_id,
                    decision:
                        ApprovalDecision::AllowOnce
                        | ApprovalDecision::AllowSession
                        | ApprovalDecision::AllowProject,
                    ..
                } if state
                    .pending_approvals
                    .get(&tool_call_id.0)
                    .and_then(|pending| pending.request.approval_diff.as_ref())
                    .is_some_and(|diff| diff.truncated) =>
                {
                    let outcome = protocol_rejection(
                        "truncated_approval_denied",
                        "a truncated diff cannot be approved; deny it and review the complete change through a bounded proposal",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::AnswerQuestion {
                    question_id,
                    answers,
                    ..
                } if (!state.pending_questions.contains_key(&question_id.0)
                    && !state.pending_model_switches.contains_key(&question_id.0))
                    || !answers.iter().any(|answer| {
                        answer.question_id == *question_id && !answer.values.is_empty()
                    }) =>
                {
                    let outcome = protocol_rejection(
                        "invalid_question_answer",
                        "question is not pending or its answer is empty",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::AnswerQuestion {
                    question_id,
                    answers,
                    ..
                } if state.pending_model_switches.contains_key(&question_id.0)
                    && model_switch_answer(answers, question_id).is_none() =>
                {
                    let outcome = protocol_rejection(
                        "invalid_model_context_transfer",
                        "model switching requires exactly one of the displayed context choices",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::AnswerQuestion { question_id, .. }
                    if state
                        .pending_model_switches
                        .get(&question_id.0)
                        .is_some_and(|pending| {
                            pending.provider.as_ref().is_some_and(|provider| {
                                !config
                                    .model
                                    .has_provider_for_alias(&pending.model.0, provider)
                            })
                        }) =>
                {
                    let outcome = protocol_rejection(
                        "unknown_provider_route",
                        "the pending model no longer has the selected provider route",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::Compact { .. } if state.running.is_some() => {
                    let outcome = protocol_rejection(
                        "turn_running",
                        "manual compaction requires an idle session",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::PinContext { item_id, .. }
                | ClientCommand::EvictContext { item_id, .. } => {
                    if state.running.is_some() {
                        let outcome = protocol_rejection(
                            "turn_running",
                            "context surgery requires an idle session",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                    if !item_id.0.starts_with("conversation:") {
                        let outcome = protocol_rejection(
                            "protected_context_item",
                            "only conversation-resident context items support pin or eviction",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                    let known = assemble_session_context(
                        config,
                        &state.conversation,
                        &state.queued,
                        &state.context_surgery,
                        &state.pruned_tool_outputs,
                        false,
                    )
                    .is_ok_and(|assembled| {
                        assembled.items.iter().any(|item| item.id.0 == item_id.0)
                    });
                    if !known {
                        let outcome = protocol_rejection(
                            "unknown_context_item",
                            "context item is not present in the current inventory",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
                ClientCommand::DumpPrompt {
                    turn_id: Some(turn_id),
                    ..
                } if parse_turn_id(turn_id).is_err()
                    || (!state
                        .turn_ends
                        .contains_key(&turn_id.0.parse::<u64>().unwrap_or(u64::MAX))
                        && state.running.as_ref().map(|running| running.id)
                            != turn_id.0.parse::<u64>().ok()) =>
                {
                    let outcome = protocol_rejection(
                        "unknown_prompt_turn",
                        "prompt dump turn must identify a known completed or active turn",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                _ => {}
            }

            if let ClientCommand::ApproveTool {
                tool_call_id,
                decision:
                    ApprovalDecision::AllowOnce
                    | ApprovalDecision::AllowSession
                    | ApprovalDecision::AllowProject,
                ..
            } = &command
            {
                let pending_request = state
                    .pending_approvals
                    .get(&tool_call_id.0)
                    .filter(|pending| pending.binding.is_some())
                    .map(|pending| (pending.request.clone(), pending.turn));
                if let Some((request, turn)) = pending_request {
                    let refreshed = if let Some(tool) = config.tools.resolve(&request.tool_name) {
                        current_approval_diff(&tool, tool_context, &request).await
                    } else {
                        Err("approved tool is no longer registered".to_owned())
                    };
                    let current_diff = refreshed.ok().flatten();
                    let current_binding = current_diff.as_ref().map(diff_binding);
                    let expected_binding = state
                        .pending_approvals
                        .get(&tool_call_id.0)
                        .and_then(|pending| pending.binding.clone());
                    if current_binding != expected_binding {
                        if let Some(diff) = current_diff {
                            let mut refreshed_request = request;
                            refreshed_request.approval_diff = Some(diff);
                            if let Some(pending) = state.pending_approvals.get_mut(&tool_call_id.0)
                            {
                                pending.binding = current_binding;
                                pending.request = refreshed_request.clone();
                            }
                            if let Err(error) = emit(
                                state,
                                events,
                                &config.event_sink,
                                PendingEvent::PermissionRequested {
                                    turn,
                                    request: refreshed_request,
                                },
                            )
                            .await
                            {
                                if let Some(pending) =
                                    state.pending_approvals.remove(&tool_call_id.0)
                                {
                                    let _ = pending.respond.send(ApprovalDecision::Deny);
                                }
                                let outcome = protocol_rejection(
                                    "approval_refresh_failed",
                                    format!("could not persist refreshed approval: {error}"),
                                );
                                send_ack(state, events, &meta, session, outcome.clone());
                                let _ = respond.send(outcome);
                                return;
                            }
                        } else if let Some(pending) =
                            state.pending_approvals.remove(&tool_call_id.0)
                        {
                            let _ = pending.respond.send(ApprovalDecision::Deny);
                        }
                        let outcome = protocol_rejection(
                            "approval_stale",
                            "workspace state changed after the displayed diff; no mutation ran and a fresh approval is required",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
            }

            if let ClientCommand::RemoveQueuedMessage { position, .. } = &command {
                let Some(index) = state
                    .queued_positions
                    .iter()
                    .position(|queued_position| queued_position.to_string() == *position)
                else {
                    let (code, message) = if state.queued.is_empty() {
                        (
                            "queued_messages_empty",
                            "there are no queued messages to remove".to_owned(),
                        )
                    } else {
                        (
                            "queued_message_not_found",
                            format!("queued message position {position:?} is no longer present"),
                        )
                    };
                    let outcome = protocol_rejection(code, message);
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                            "queued message removal failed".to_owned(),
                        )));
                    }
                    return;
                };
                let queued_position = state.queued_positions[index];
                state.transient_cause = Some(meta.request_id.clone());
                let persisted = emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::QueuedMessageRemoved {
                        position: queued_position,
                    },
                )
                .await;
                state.transient_cause = None;
                match persisted {
                    Ok(()) => {
                        state.queued.remove(index);
                        state.queued_positions.remove(index);
                        let accepted = CommandOutcome::Accepted;
                        send_ack(state, events, &meta, session, accepted.clone());
                        let _ = respond.send(accepted);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Ok(ProtocolCompletion::Unit));
                        }
                    }
                    Err(error) => {
                        let outcome = protocol_rejection(
                            "session_persistence_failure",
                            format!("could not persist queued message removal: {error}"),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Err(error));
                        }
                    }
                }
                return;
            }

            if matches!(&command, ClientCommand::ClearQueuedMessages { .. }) {
                if state.queued.is_empty() {
                    let outcome = protocol_rejection(
                        "queued_messages_empty",
                        "there are no queued messages to clear",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                            "queued message clear failed".to_owned(),
                        )));
                    }
                    return;
                }
                state.transient_cause = Some(meta.request_id.clone());
                let persisted = emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::QueuedMessagesCleared,
                )
                .await;
                state.transient_cause = None;
                match persisted {
                    Ok(()) => {
                        state.queued.clear();
                        state.queued_positions.clear();
                        let accepted = CommandOutcome::Accepted;
                        send_ack(state, events, &meta, session, accepted.clone());
                        let _ = respond.send(accepted);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Ok(ProtocolCompletion::Unit));
                        }
                    }
                    Err(error) => {
                        let outcome = protocol_rejection(
                            "session_persistence_failure",
                            format!("could not persist queued message clear: {error}"),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Err(error));
                        }
                    }
                }
                return;
            }

            if matches!(
                &command,
                ClientCommand::ListPermissions { .. }
                    | ClientCommand::AddSessionPermissionRule { .. }
                    | ClientCommand::RemoveSessionPermissionRule { .. }
                    | ClientCommand::RevokePermissionApproval { .. }
            ) {
                let mutating = !matches!(&command, ClientCommand::ListPermissions { .. });
                let result = if mutating
                    && (state.running.is_some()
                        || state.active_shell.is_some()
                        || config.tools.session_activity(&state.session_id).is_some())
                {
                    Err("permission mutations require an idle session".to_owned())
                } else {
                    apply_permission_command(&command, &config.permissions)
                };
                match result {
                    Ok(permissions) => {
                        let accepted = CommandOutcome::Accepted;
                        send_ack(state, events, &meta, session, accepted.clone());
                        send_connection_event(
                            events,
                            &meta.client_id,
                            EngineEvent::PermissionsListed {
                                meta: query_meta(state, &meta),
                                session_id: state.session_id.clone(),
                                permissions,
                            },
                        );
                        let _ = respond.send(accepted);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Ok(ProtocolCompletion::Unit));
                        }
                    }
                    Err(message) => {
                        let outcome = protocol_rejection("permission_operation_failed", message);
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                                "permission operation failed".to_owned(),
                            )));
                        }
                    }
                }
                return;
            }

            let attach_gap = if let ClientCommand::AttachSession {
                last_seen_sequence, ..
            } = &command
            {
                let tail = match config.event_sink.last_sequence().await {
                    Ok(tail) => tail,
                    Err(error) => {
                        let outcome = protocol_rejection(
                            "gap_replay_failed",
                            format!("could not read durable session tail: {error}"),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                };
                if last_seen_sequence
                    .is_some_and(|last_seen| tail.is_none_or(|tail| last_seen > tail))
                {
                    let outcome = protocol_rejection(
                        "sequence_ahead_of_log",
                        "last-seen sequence is ahead of the durable session tail",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                match config.event_sink.read_after(*last_seen_sequence).await {
                    Ok(gap) => {
                        if let Err(error) =
                            validate_gap(*last_seen_sequence, &gap, &config.session_id)
                        {
                            let outcome = protocol_rejection(
                                "invalid_gap_replay",
                                format!("durable session gap is invalid: {error}"),
                            );
                            send_ack(state, events, &meta, session, outcome.clone());
                            let _ = respond.send(outcome);
                            return;
                        }
                        Some(gap)
                    }
                    Err(error) => {
                        let outcome = protocol_rejection(
                            "gap_replay_failed",
                            format!("could not read durable session gap: {error}"),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
            } else {
                None
            };

            state.transient_cause = Some(meta.request_id.clone());
            let lease_persist = match &command {
                ClientCommand::AttachSession { role, .. }
                    if *role == ClientRole::Driver && state.driver_client_id.is_none() =>
                {
                    let driver_event = if state.sequence.is_none() {
                        PendingEvent::SessionCreated {
                            driver_client_id: meta.client_id.clone(),
                        }
                    } else {
                        PendingEvent::DriverChanged {
                            driver_client_id: meta.client_id.clone(),
                        }
                    };
                    emit(state, events, &config.event_sink, driver_event).await
                }
                ClientCommand::TakeDriver { .. }
                    if state.driver_client_id.as_ref() != Some(&meta.client_id) =>
                {
                    emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::DriverChanged {
                            driver_client_id: meta.client_id.clone(),
                        },
                    )
                    .await
                }
                _ => Ok(()),
            };
            if let Err(error) = lease_persist {
                state.transient_cause = None;
                let outcome = protocol_rejection(
                    "session_persistence_failure",
                    format!("could not persist the driver lease: {error}"),
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Err(error));
                }
                return;
            }
            if let Some(gap) = &attach_gap {
                for event in gap {
                    let _ = events.send(RoutedEvent {
                        target: Some(meta.client_id.clone()),
                        event: event.clone(),
                    });
                }
            }
            let mut precommitted_answer = None;
            if let ClientCommand::AnswerQuestion {
                question_id,
                answers,
                ..
            } = &command
            {
                let answer = answers
                    .iter()
                    .find(|answer| answer.question_id == *question_id)
                    .map(|answer| answer.values.join("\n"))
                    .unwrap_or_default();
                let pending = if let Some(pending) = state.pending_questions.remove(&question_id.0)
                {
                    PrecommittedAnswer::Turn(pending, answer)
                } else if let Some(pending) = state.pending_model_switches.remove(&question_id.0) {
                    let Some(strategy) = model_switch_answer(answers, question_id) else {
                        state
                            .pending_model_switches
                            .insert(question_id.0.clone(), pending);
                        state.transient_cause = None;
                        let outcome = protocol_rejection(
                            "invalid_model_context_transfer",
                            "model context choice stopped being valid before commit",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    };
                    PrecommittedAnswer::Model(pending, strategy)
                } else {
                    state.transient_cause = None;
                    let outcome = protocol_rejection(
                        "invalid_question_answer",
                        "question stopped pending before its answer could be committed",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                };
                let turn = match &pending {
                    PrecommittedAnswer::Turn(pending, _) => pending.turn,
                    PrecommittedAnswer::Model(pending, _) => pending.turn,
                };
                if let Err(error) = emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::QuestionAnswered {
                        turn,
                        question_id: question_id.clone(),
                        answers: answers.clone(),
                    },
                )
                .await
                {
                    if let PrecommittedAnswer::Turn(pending, _) = pending {
                        drop(pending.respond);
                    }
                    state.transient_cause = None;
                    if recover_actor_from_journal(state, config, events, active_turn)
                        .await
                        .is_err()
                    {
                        // The durable log itself could not be read or repaired;
                        // unlike an append failure, continuing from mutable
                        // memory would risk acknowledging nonexistent state.
                        state.poisoned = true;
                    }
                    let outcome = protocol_rejection(
                        "session_persistence_failure",
                        format!("could not persist the question answer: {error}"),
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Err(error));
                    }
                    return;
                }
                precommitted_answer = Some(pending);
            }
            if matches!(
                command,
                ClientCommand::GetSessionReview { .. } | ClientCommand::ReviewFile { .. }
            ) {
                let result = match &command {
                    ClientCommand::GetSessionReview { .. } => config
                        .checkpoints
                        .session_review(&state.session_id)
                        .await
                        .map(|review| EngineEvent::SessionReviewReady {
                            meta: query_meta(state, &meta),
                            session_id: state.session_id.clone(),
                            review,
                        }),
                    ClientCommand::ReviewFile {
                        path,
                        decision,
                        current_hash,
                        ..
                    } => config
                        .checkpoints
                        .resolve_review_file(
                            &state.session_id,
                            Path::new(path),
                            *decision,
                            current_hash,
                        )
                        .await
                        .map(|review| EngineEvent::SessionReviewUpdated {
                            meta: query_meta(state, &meta),
                            session_id: state.session_id.clone(),
                            path: path.clone(),
                            decision: *decision,
                            review,
                        }),
                    _ => unreachable!("review command guard narrows the command"),
                };
                state.transient_cause = None;
                match result {
                    Ok(event) => {
                        let accepted = CommandOutcome::Accepted;
                        send_ack(state, events, &meta, session, accepted.clone());
                        send_connection_event(events, &meta.client_id, event);
                        let _ = respond.send(accepted);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Ok(ProtocolCompletion::Unit));
                        }
                    }
                    Err(error) => {
                        let outcome = protocol_rejection(
                            "session_review_failed",
                            "session review could not be completed; refresh and retry",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Err(error));
                        }
                    }
                }
                return;
            }
            if matches!(
                command,
                ClientCommand::AttachDevelopmentPlugin { .. }
                    | ClientCommand::DetachDevelopmentPlugin { .. }
            ) {
                let current = SessionExtensionSnapshot {
                    revision: config.workspace_generation,
                    workspace_roots: Arc::from(
                        std::iter::once(config.workspace_root.clone())
                            .chain(config.additional_workspace_roots.iter().cloned())
                            .collect::<Vec<_>>(),
                    ),
                    tools: Arc::clone(&config.tools),
                    hooks: Arc::clone(&config.hooks),
                    commands: Arc::clone(&config.commands),
                };
                let prepared = match &command {
                    ClientCommand::AttachDevelopmentPlugin { source, .. } => {
                        config
                            .extension_development
                            .attach(Path::new(source), current)
                            .await
                    }
                    ClientCommand::DetachDevelopmentPlugin { .. } => {
                        config.extension_development.detach().await
                    }
                    _ => unreachable!("development command guard narrows the command"),
                };
                match prepared {
                    Ok(snapshot) => {
                        let next_config = Arc::new(config.with_extension_snapshot(&snapshot));
                        *command_descriptors
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::from(
                            next_config
                                .commands
                                .descriptors()
                                .cloned()
                                .collect::<Vec<_>>(),
                        );
                        *config = next_config;
                        let accepted = CommandOutcome::Accepted;
                        send_ack(state, events, &meta, session, accepted.clone());
                        let _ = respond.send(accepted);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Ok(ProtocolCompletion::Unit));
                        }
                    }
                    Err(error) => {
                        let outcome =
                            protocol_rejection("development_plugin_rejected", error.to_string());
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Err(error));
                        }
                    }
                }
                return;
            }
            let accepted = CommandOutcome::Accepted;
            send_ack(state, events, &meta, session, accepted.clone());
            let _ = respond.send(accepted);
            match command {
                ClientCommand::ListPermissions { .. }
                | ClientCommand::AddSessionPermissionRule { .. }
                | ClientCommand::RemoveSessionPermissionRule { .. }
                | ClientCommand::RemoveQueuedMessage { .. }
                | ClientCommand::ClearQueuedMessages { .. }
                | ClientCommand::ExportSession { .. }
                | ClientCommand::RevokePermissionApproval { .. }
                | ClientCommand::ListMcpServers { .. }
                | ClientCommand::ListRuntimeServices { .. }
                | ClientCommand::AddMcpHttpServer { .. }
                | ClientCommand::AddMcpStdioServer { .. }
                | ClientCommand::RemoveMcpServer { .. }
                | ClientCommand::ReviewMcpServer { .. }
                | ClientCommand::ApproveMcpServer { .. }
                | ClientCommand::SetMcpServerEnabled { .. } => {
                    unreachable!("host query commands return through their typed query branch")
                }
                ClientCommand::RenameSession { title, .. } => {
                    state.transient_cause = Some(meta.request_id.clone());
                    let result = emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::SessionTitleUpdated {
                            title: title.clone(),
                            usage: None,
                            cost: None,
                        },
                    )
                    .await;
                    state.transient_cause = None;
                    if result.is_ok() {
                        state.session_title = Some(title);
                        state.title_generation_started = true;
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::AttachDevelopmentPlugin { .. }
                | ClientCommand::DetachDevelopmentPlugin { .. } => {
                    unreachable!("development plugin commands return through their typed branch")
                }
                ClientCommand::AttachSession { role, .. } => {
                    state
                        .client_roles
                        .insert(meta.client_id.0.clone(), role.clone());
                    if role == ClientRole::Driver && state.driver_client_id.is_none() {
                        state.driver_client_id = Some(meta.client_id.clone());
                    }
                }
                ClientCommand::TakeDriver { .. } => {
                    state.driver_client_id = Some(meta.client_id.clone());
                    state
                        .client_roles
                        .insert(meta.client_id.0.clone(), ClientRole::Driver);
                }
                ClientCommand::SwitchMode { mode, .. } => {
                    let result =
                        apply_mode_change(state, events, &config.event_sink, mode, &config.modes)
                            .await;
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::ApprovePlan {
                    decision,
                    revisions,
                    ..
                } => {
                    let execute_definition = if decision == PlanDecision::Approve {
                        let Some(definition) = config.modes.get("execute") else {
                            if let Some(complete) = completion.take() {
                                let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                                    "execute mode is not registered".to_owned(),
                                )));
                            }
                            return;
                        };
                        Some(definition)
                    } else {
                        None
                    };
                    let artifact = state.pending_plan.clone().unwrap_or_else(|| PlanArtifact {
                        title: String::new(),
                        summary_md: String::new(),
                        steps: Vec::new(),
                        open_questions: Vec::new(),
                    });
                    let mut durable = vec![PendingEvent::PlanReviewed {
                        artifact: artifact.clone(),
                        decision,
                        revisions: revisions.clone(),
                    }];
                    let context_turn =
                        plan_review_context_turn(&artifact, decision, revisions.as_deref());
                    let item_id =
                        ContextItemId(format!("conversation:{}", state.conversation.len()));
                    if let Some(turn) = context_turn.clone() {
                        durable.push(PendingEvent::ConversationTurnCommitted {
                            agent_turn: state.completed_turns,
                            turn,
                        });
                    }
                    if let Some(definition) = execute_definition {
                        durable.push(PendingEvent::ContextItemPinned {
                            item_id: item_id.clone(),
                            effective_after_agent_turn: state.completed_turns,
                        });
                        durable.push(PendingEvent::ModeChanged {
                            mode: ModeId("execute".to_owned()),
                            definition_fingerprint: definition.semantic_fingerprint(),
                        });
                    }
                    let result = emit_batch(state, events, &config.event_sink, durable).await;
                    if result.is_ok() {
                        state.pending_plan = None;
                        if let Some(turn) = context_turn {
                            state.conversation.push(turn);
                        }
                        if let Some(definition) = execute_definition {
                            state.approved_plan = Some(artifact);
                            state.plan_gate_active = false;
                            state.context_surgery.push(ContextSurgeryAction {
                                item_id,
                                pinned: true,
                                effective_after_agent_turn: state.completed_turns,
                            });
                            state.mode = mode_permission_base(definition);
                            state.mode_id = ModeId("execute".to_owned());
                        }
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::SwitchModel {
                    model, provider, ..
                } => {
                    let thinking = config.model.thinking_for_model(&model.0, state.thinking);
                    let prepared = PreparedModelSwitch {
                        model: model.clone(),
                        provider: provider.clone(),
                        thinking,
                    };
                    let has_prior_context = state
                        .conversation
                        .iter()
                        .any(|turn| turn.role != Role::System);
                    let result = if has_prior_context
                        && (state.model_alias != model.0 || state.provider != provider)
                    {
                        let question_id =
                            QuestionId(format!("model-switch-{}", state.next_question));
                        state.next_question = state.next_question.saturating_add(1);
                        let question = model_switch_question(
                            question_id.clone(),
                            model.clone(),
                            provider.clone(),
                        );
                        let result = emit(
                            state,
                            events,
                            &config.event_sink,
                            PendingEvent::QuestionAsked {
                                turn: state.completed_turns,
                                question_id: question_id.clone(),
                                questions: vec![question],
                            },
                        )
                        .await;
                        if result.is_ok() {
                            state.pending_model_switches.insert(
                                question_id.0,
                                PendingModelSwitch {
                                    turn: state.completed_turns,
                                    model,
                                    provider,
                                },
                            );
                        }
                        result
                    } else {
                        commit_prepared_model_switch(state, config, events, prepared, false).await
                    };
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::UserShellStarted { command, .. } => {
                    let shell_id = ShellId(format!(
                        "shell-{}",
                        state
                            .sequence
                            .map_or(0, |sequence| sequence.saturating_add(1))
                    ));
                    let shell = RecoveredUserShell {
                        shell_id: shell_id.clone(),
                        command: command.clone(),
                    };
                    let result = emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::UserShellStateChanged {
                            shell_id,
                            command,
                            active: true,
                            status: None,
                            captured_output: None,
                        },
                    )
                    .await;
                    if result.is_ok() {
                        state.active_shell = Some(shell);
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::UserShellEnded {
                    shell_id,
                    status,
                    captured_output,
                    ..
                } => {
                    let command = state
                        .active_shell
                        .as_ref()
                        .map(|shell| shell.command.clone())
                        .unwrap_or_default();
                    let context = shell_context_turn(&command, status, captured_output.as_deref());
                    let result = emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::UserShellStateChanged {
                            shell_id,
                            command,
                            active: false,
                            status: Some(status),
                            captured_output,
                        },
                    )
                    .await;
                    if result.is_ok() {
                        state.conversation.push(context);
                        state.active_shell = None;
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::SendMessage {
                    content,
                    attachments,
                    ..
                } => {
                    let (internal_respond, internal_receive) = oneshot::channel();
                    Box::pin(handle_actor_command(
                        ActorCommand::SendMessage {
                            command_meta: meta,
                            content,
                            attachments,
                            observed_turn: active_turn.load(Ordering::Acquire),
                            respond: internal_respond,
                        },
                        state,
                        config,
                        tool_context,
                        turn_signals,
                        events,
                        active_turn,
                        command_descriptors,
                        mode_registry,
                    ))
                    .await;
                    match internal_receive.await {
                        Ok(Ok(disposition)) => {
                            if let Some(complete) = completion.take() {
                                let _ = complete.send(Ok(ProtocolCompletion::Message(disposition)));
                            }
                        }
                        Ok(Err(error)) => {
                            if let Some(complete) = completion.take() {
                                let _ = complete.send(Err(error.clone()));
                            }
                            // A turn-opening failure is not itself evidence that
                            // durable state is inconsistent. Production sinks
                            // append opening batches atomically, and validation
                            // failures happen before an append. Keep the actor
                            // usable so a transient storage failure (or a
                            // corrected input) can be retried without trapping
                            // the UI in an unrecoverable live-poisoned session.
                            let _ = emit(
                                state,
                                events,
                                &config.event_sink,
                                PendingEvent::Error {
                                    message: format!(
                                        "accepted message failed before turn execution: {error}"
                                    ),
                                },
                            )
                            .await;
                        }
                        Err(_) => {
                            if let Some(complete) = completion.take() {
                                let _ = complete.send(Err(AgentLoopError::Closed));
                            }
                        }
                    }
                }
                ClientCommand::Interrupt { .. } => {
                    if let Some(running) = &state.running {
                        running.cancellation.cancel();
                    }
                }
                ClientCommand::ApproveTool {
                    tool_call_id,
                    decision,
                    ..
                } => {
                    if let Some(pending) = state.pending_approvals.remove(&tool_call_id.0) {
                        let _ = pending.respond.send(decision);
                    }
                }
                ClientCommand::AnswerQuestion { .. } => {
                    if let Some(answer) = precommitted_answer.take() {
                        match answer {
                            PrecommittedAnswer::Turn(pending, answer) => {
                                let _ = pending.respond.send(answer);
                                if let Some(complete) = completion.take() {
                                    let _ = complete.send(Ok(ProtocolCompletion::Unit));
                                }
                            }
                            PrecommittedAnswer::Model(pending, strategy) => {
                                let prepared = PreparedModelSwitch {
                                    thinking: config
                                        .model
                                        .thinking_for_model(&pending.model.0, state.thinking),
                                    model: pending.model,
                                    provider: pending.provider,
                                };
                                match strategy {
                                    ModelContextTransfer::PassSummary => {
                                        let completion = completion.take();
                                        start_manual_compaction(
                                            state,
                                            config,
                                            turn_signals,
                                            active_turn,
                                            Some(
                                                "Summarize the conversation for transfer to the selected model. Preserve user intent, decisions, constraints, and unfinished work."
                                                    .to_owned(),
                                            ),
                                            Some(prepared),
                                            completion,
                                        );
                                    }
                                    ModelContextTransfer::PassFullContext
                                    | ModelContextTransfer::StartWithoutContext => {
                                        let clear_context =
                                            strategy == ModelContextTransfer::StartWithoutContext;
                                        let result = match config
                                            .model
                                            .prepare_model(&prepared.model.0)
                                            .await
                                        {
                                            Ok(()) => {
                                                commit_prepared_model_switch(
                                                    state,
                                                    config,
                                                    events,
                                                    prepared,
                                                    clear_context,
                                                )
                                                .await
                                            }
                                            Err(error) => Err(error),
                                        };
                                        if let Some(complete) = completion.take() {
                                            let _ = complete
                                                .send(result.map(|()| ProtocolCompletion::Unit));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                ClientCommand::Rewind {
                    target: RewindTarget::Turn { turn_id },
                    ..
                } => {
                    let rewind = match parse_turn_id(&turn_id) {
                        Ok(to_turn) => rewind_state(state, config, events, to_turn).await,
                        Err(error) => Err(AgentLoopError::InvalidConfiguration(error.to_string())),
                    };
                    let result = match rewind {
                        Ok(unrestorable_paths) => {
                            let message = if unrestorable_paths.is_empty() {
                                format!("rewound to turn {}", turn_id.0)
                            } else {
                                format!(
                                    "rewound to turn {} with {} unrestorable path(s)",
                                    turn_id.0,
                                    unrestorable_paths.len()
                                )
                            };
                            emit(
                                state,
                                events,
                                &config.event_sink,
                                PendingEvent::CommandFinished {
                                    name: "rewind".to_owned(),
                                    message,
                                    unrestorable_paths: unrestorable_paths.clone(),
                                },
                            )
                            .await
                            .map(|()| unrestorable_paths)
                        }
                        Err(error) => Err(error),
                    };
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(ProtocolCompletion::Rewind));
                    }
                }
                ClientCommand::PinContext { item_id, .. } => {
                    let result =
                        apply_context_surgery(state, events, &config.event_sink, item_id, true)
                            .await;
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::EvictContext { item_id, .. } => {
                    let result =
                        apply_context_surgery(state, events, &config.event_sink, item_id, false)
                            .await;
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::GetContext { .. } => {
                    let result = assemble_session_context(
                        config,
                        &state.conversation,
                        &state.queued,
                        &state.context_surgery,
                        &state.pruned_tool_outputs,
                        false,
                    )
                    .map(|assembled| {
                        context_snapshot(
                            &assembled,
                            &state.conversation,
                            &state.pruned_tool_outputs,
                            config.model.context_metadata(&config.model_alias),
                            &config.model.compaction_config(),
                            state
                                .running
                                .as_ref()
                                .map(|running| wire_turn_id(running.id)),
                        )
                    });
                    if let Ok(snapshot) = &result {
                        send_connection_event(
                            events,
                            &meta.client_id,
                            EngineEvent::ContextSnapshotReady {
                                meta: query_meta(state, &meta),
                                session_id: state.session_id.clone(),
                                snapshot: snapshot.clone(),
                            },
                        );
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(ProtocolCompletion::Context));
                    }
                }
                ClientCommand::GetCost { .. } => {
                    let result = build_cost_snapshot(state, config).await;
                    if let Ok(snapshot) = &result {
                        send_connection_event(
                            events,
                            &meta.client_id,
                            EngineEvent::CostSnapshotReady {
                                meta: query_meta(state, &meta),
                                session_id: state.session_id.clone(),
                                snapshot: snapshot.clone(),
                            },
                        );
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(ProtocolCompletion::Cost));
                    }
                }
                ClientCommand::DumpPrompt { turn_id, .. } => {
                    let historical = if let Some(requested) = &turn_id {
                        let events = config.event_sink.read_after(None).await;
                        events.and_then(|events| {
                            let boundary = events.iter().position(|event| {
                                matches!(
                                    event,
                                    EngineEvent::ContextUsageUpdated { turn_id, .. }
                                        if turn_id == requested
                                )
                            });
                            let boundary = boundary.ok_or_else(|| {
                                AgentLoopError::InvalidConfiguration(format!(
                                    "no assembled prompt was recorded for turn {}",
                                    requested.0
                                ))
                            })?;
                            project_session_events_with_modes(&events[..=boundary], &config.modes)
                                .map_err(|error| AgentLoopError::Persistence(error.to_string()))
                        })
                    } else {
                        Ok(SessionRecoveredState {
                            conversation: state.conversation.clone(),
                            queued_messages: state.queued.iter().cloned().collect(),
                            queued_message_positions: state
                                .queued_positions
                                .iter()
                                .copied()
                                .collect(),
                            context_surgery: state.context_surgery.clone(),
                            pruned_tool_outputs: state.pruned_tool_outputs.clone(),
                            accounting: state.accounting.clone(),
                            ..SessionRecoveredState::default()
                        })
                    };
                    let result = historical.and_then(|historical| {
                        assemble_session_context(
                            config,
                            &historical.conversation,
                            &historical.queued_messages.iter().cloned().collect(),
                            &historical.context_surgery,
                            &historical.pruned_tool_outputs,
                            true,
                        )
                        .map(|assembled| prompt_dump(&assembled, &config.model_alias, turn_id))
                    });
                    if let Ok(dump) = &result {
                        send_connection_event(
                            events,
                            &meta.client_id,
                            EngineEvent::PromptDumpReady {
                                meta: query_meta(state, &meta),
                                session_id: state.session_id.clone(),
                                dump: dump.clone(),
                            },
                        );
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(ProtocolCompletion::Prompt));
                    }
                }
                ClientCommand::Compact { instructions, .. } => {
                    let completion = completion.take();
                    start_manual_compaction(
                        state,
                        config,
                        turn_signals,
                        active_turn,
                        instructions,
                        None,
                        completion,
                    );
                }
                ClientCommand::CreateSession { .. }
                | ClientCommand::ResumeSession { .. }
                | ClientCommand::Fork { .. }
                | ClientCommand::GetSessionReview { .. }
                | ClientCommand::ReviewFile { .. }
                | ClientCommand::ListSessions { .. }
                | ClientCommand::SearchSessions { .. }
                | ClientCommand::ListCommands { .. }
                | ClientCommand::ListModes { .. }
                | ClientCommand::ListModels { .. }
                | ClientCommand::ListSettings { .. }
                | ClientCommand::SetSetting { .. }
                | ClientCommand::BeginProviderAuth { .. }
                | ClientCommand::ConfigureBuiltinProvider { .. }
                | ClientCommand::CompleteProviderAuth { .. }
                | ClientCommand::CancelProviderAuth { .. }
                | ClientCommand::SearchWorkspaceFiles { .. }
                | ClientCommand::PreviewWorkspaceFile { .. }
                | ClientCommand::GetWorkspaceStatus { .. }
                | ClientCommand::GetWorkspaceDiff { .. }
                | ClientCommand::ListSubagents { .. }
                | ClientCommand::ReplaySubagent { .. }
                | ClientCommand::ContinueSubagent { .. }
                | ClientCommand::InterruptSubagent { .. }
                | ClientCommand::CloseSubagent { .. }
                | ClientCommand::ShutdownHost { .. }
                | ClientCommand::Rewind {
                    target: RewindTarget::Checkpoint { .. },
                    ..
                } => {}
            }
            if let Some(complete) = completion.take() {
                let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                    "command has no local completion result".to_owned(),
                )));
            }
            state.transient_cause = None;
        }
        ActorCommand::PluginInjectMessage {
            plugin_id,
            content,
            respond,
        } => {
            let result = handle_plugin_message(
                plugin_id,
                content,
                state,
                StartTurnRuntime {
                    config,
                    tool_context,
                    signals: turn_signals,
                    events,
                    active_turn,
                },
            )
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::PluginSetStatus {
            plugin_id,
            status,
            respond,
        } => {
            let result = async {
                validate_plugin_id(&plugin_id)?;
                validate_plugin_text("plugin status", &status, MAX_PLUGIN_STATUS_BYTES)?;
                if state.poisoned {
                    return Err(AgentLoopError::InvalidConfiguration(
                        "session requires recovery before plugin status updates".to_owned(),
                    ));
                }
                let status = config.secret_redactor.redact(&status);
                validate_plugin_text("redacted plugin status", &status, MAX_PLUGIN_STATUS_BYTES)?;
                emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::PluginStatusChanged { plugin_id, status },
                )
                .await
            }
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::PluginNotify {
            plugin_id,
            title,
            message,
            respond,
        } => {
            let result = async {
                validate_plugin_id(&plugin_id)?;
                validate_plugin_text(
                    "notification title",
                    &title,
                    MAX_PLUGIN_NOTIFICATION_TITLE_BYTES,
                )?;
                validate_plugin_text(
                    "notification message",
                    &message,
                    MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES,
                )?;
                if state.poisoned {
                    return Err(AgentLoopError::InvalidConfiguration(
                        "session requires recovery before plugin notifications".to_owned(),
                    ));
                }
                let title = config.secret_redactor.redact(&title);
                let message = config.secret_redactor.redact(&message);
                validate_plugin_text(
                    "redacted notification title",
                    &title,
                    MAX_PLUGIN_NOTIFICATION_TITLE_BYTES,
                )?;
                validate_plugin_text(
                    "redacted notification message",
                    &message,
                    MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES,
                )?;
                emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::UiNotification {
                        plugin_id,
                        title,
                        message,
                    },
                )
                .await
            }
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::SendMessage {
            command_meta,
            content,
            attachments,
            observed_turn,
            respond,
        } => {
            if content.trim_start().starts_with('/') {
                let mut context = SessionCommandContext {
                    running: state.running.is_some() || state.initialization_running,
                    queued_messages: state.queued.len(),
                    mode: state.mode,
                    mode_id: state.mode_id.clone(),
                    modes: Arc::clone(&config.modes),
                    permission_summary: render_permission_snapshot(&config.permissions.snapshot()),
                    plan_summary: state
                        .pending_plan
                        .as_ref()
                        .or(state.approved_plan.as_ref())
                        .map_or_else(|| "no plan has been submitted".to_owned(), render_plan),
                    command_summary: config
                        .commands
                        .descriptors()
                        .map(|descriptor| {
                            descriptor.argument_hint().map_or_else(
                                || format!("/{} — {}", descriptor.name(), descriptor.description()),
                                |hint| {
                                    format!(
                                        "/{} {} — {}",
                                        descriptor.name(),
                                        hint,
                                        descriptor.description()
                                    )
                                },
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                let result = config.commands.dispatch_line(&mut context, &content).await;
                let disposition = match result {
                    Ok(mut output) => {
                        let mut unrestorable_paths = Vec::new();
                        let mut submitted_prompt = None;
                        let mut deferred_command_completion = false;
                        match output.action {
                            SessionCommandAction::Interrupt => {
                                if let Some(running) = &state.running
                                    && running.id == observed_turn
                                {
                                    running.cancellation.cancel();
                                }
                            }
                            SessionCommandAction::Rewind { to_turn } => {
                                match rewind_state(state, config, events, to_turn).await {
                                    Ok(report) => unrestorable_paths = report,
                                    Err(_error) => {
                                        let _ = respond.send(Err(
                                            AgentLoopError::InvalidConfiguration(
                                                "workspace root generation could not prepare"
                                                    .to_owned(),
                                            ),
                                        ));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::Review => {
                                match config.checkpoints.session_review(&state.session_id).await {
                                    Ok(review) => {
                                        send_connection_event(
                                            events,
                                            &command_meta.client_id,
                                            EngineEvent::SessionReviewReady {
                                                meta: query_meta(state, &command_meta),
                                                session_id: state.session_id.clone(),
                                                review: review.clone(),
                                            },
                                        );
                                        output.message = render_session_review(&review);
                                    }
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::Context => {
                                let snapshot = assemble_session_context(
                                    config,
                                    &state.conversation,
                                    &state.queued,
                                    &state.context_surgery,
                                    &state.pruned_tool_outputs,
                                    false,
                                )
                                .map(|assembled| {
                                    context_snapshot(
                                        &assembled,
                                        &state.conversation,
                                        &state.pruned_tool_outputs,
                                        config.model.context_metadata(&config.model_alias),
                                        &config.model.compaction_config(),
                                        state
                                            .running
                                            .as_ref()
                                            .map(|running| wire_turn_id(running.id)),
                                    )
                                });
                                match snapshot {
                                    Ok(snapshot) => {
                                        send_connection_event(
                                            events,
                                            &command_meta.client_id,
                                            EngineEvent::ContextSnapshotReady {
                                                meta: query_meta(state, &command_meta),
                                                session_id: state.session_id.clone(),
                                                snapshot: snapshot.clone(),
                                            },
                                        );
                                        output.message = render_context_snapshot(&snapshot);
                                    }
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::PinContext { item_id } => {
                                if let Err(error) = apply_registered_context_surgery(
                                    state,
                                    config,
                                    events,
                                    item_id.clone(),
                                    true,
                                )
                                .await
                                {
                                    let _ = respond.send(Err(error));
                                    return;
                                }
                                output.message = format!("pinned {}", item_id.0);
                            }
                            SessionCommandAction::EvictContext { item_id } => {
                                if let Err(error) = apply_registered_context_surgery(
                                    state,
                                    config,
                                    events,
                                    item_id.clone(),
                                    false,
                                )
                                .await
                                {
                                    let _ = respond.send(Err(error));
                                    return;
                                }
                                output.message = format!("evicted {}", item_id.0);
                            }
                            SessionCommandAction::Cost => {
                                match build_cost_snapshot(state, config).await {
                                    Ok(snapshot) => {
                                        send_connection_event(
                                            events,
                                            &command_meta.client_id,
                                            EngineEvent::CostSnapshotReady {
                                                meta: query_meta(state, &command_meta),
                                                session_id: state.session_id.clone(),
                                                snapshot: snapshot.clone(),
                                            },
                                        );
                                        output.message = render_cost_snapshot(&snapshot);
                                    }
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::Compact { instructions } => {
                                start_manual_compaction(
                                    state,
                                    config,
                                    turn_signals,
                                    active_turn,
                                    instructions,
                                    None,
                                    None,
                                );
                            }
                            SessionCommandAction::SwitchMode { mode } => {
                                let base = config
                                    .modes
                                    .get(&mode.0)
                                    .map(mode_permission_base)
                                    .ok_or_else(|| {
                                        AgentLoopError::InvalidConfiguration(format!(
                                            "unknown mode {:?}",
                                            mode.0
                                        ))
                                    });
                                let base = match base {
                                    Ok(base) => base,
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                };
                                if base == SessionMode::Execute && state.plan_gate_active {
                                    let _ = respond.send(Err(
                                        AgentLoopError::InvalidConfiguration(
                                            "plan_approval_required: submit and approve a plan before Execute"
                                                .to_owned(),
                                        ),
                                    ));
                                    return;
                                }
                                if let Err(error) = apply_mode_change(
                                    state,
                                    events,
                                    &config.event_sink,
                                    mode,
                                    &config.modes,
                                )
                                .await
                                {
                                    let _ = respond.send(Err(error));
                                    return;
                                }
                            }
                            SessionCommandAction::SetPermissionMode { mode } => {
                                if let Err(error) =
                                    apply_permission_mode_change(state, events, config, mode).await
                                {
                                    let _ = respond.send(Err(error));
                                    return;
                                }
                                output.message =
                                    render_permission_snapshot(&config.permissions.snapshot());
                            }
                            SessionCommandAction::AddPermissionRule { rule } => {
                                if let Err(message) =
                                    config.permissions.add_session_rule(rule.clone())
                                {
                                    let _ = respond
                                        .send(Err(AgentLoopError::InvalidConfiguration(message)));
                                    return;
                                }
                                output.message = format!(
                                    "added session permission rule: {:?} {}",
                                    rule.action, rule.pattern
                                );
                            }
                            SessionCommandAction::RemovePermissionRule { pattern } => {
                                output.message = if config.permissions.remove_session_rule(&pattern)
                                {
                                    format!("removed session permission rule: {pattern}")
                                } else {
                                    format!("no session permission rule matched: {pattern}")
                                };
                            }
                            SessionCommandAction::ClearSessionPermissions => {
                                let cleared = config.permissions.clear_session_permissions();
                                output.message = format!(
                                    "cleared {} session permission rule(s) and {} remembered approval(s)",
                                    cleared.rules, cleared.approvals
                                );
                            }
                            SessionCommandAction::ListPermissionApprovals => {
                                output.message = render_permission_approvals(
                                    &config.permissions.approval_snapshot(),
                                );
                            }
                            SessionCommandAction::RevokeSessionApprovals { id } => {
                                let removed =
                                    config.permissions.revoke_session_approvals(id.as_deref());
                                output.message = format!("revoked {removed} session approval(s)");
                            }
                            SessionCommandAction::RevokeProjectApprovals { id } => {
                                match config.permissions.revoke_project_approvals(id.as_deref()) {
                                    Ok(removed) => {
                                        output.message =
                                            format!("revoked {removed} project approval(s)");
                                    }
                                    Err(error) => {
                                        let _ = respond.send(Err(
                                            AgentLoopError::InvalidConfiguration(format!(
                                                "project approval revocation failed: {error}"
                                            )),
                                        ));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::AddWorkspaceRoot { path } => {
                                let current_roots = std::iter::once(config.workspace_root.clone())
                                    .chain(config.additional_workspace_roots.iter().cloned())
                                    .collect::<Vec<_>>();
                                let generation = match config
                                    .workspace_roots
                                    .append_root(
                                        &path,
                                        &current_roots,
                                        config.workspace_generation,
                                        state.next_turn,
                                        Arc::clone(&config.permissions),
                                    )
                                    .await
                                {
                                    Ok(generation) => generation,
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                };
                                let valid_append = generation.generation
                                    == config.workspace_generation.saturating_add(1)
                                    && generation.effective_from_turn == state.next_turn
                                    && generation.roots.len() == current_roots.len() + 1
                                    && generation
                                        .roots
                                        .iter()
                                        .take(current_roots.len())
                                        .eq(current_roots.iter())
                                    && generation.roots.iter().all(|root| {
                                        std::fs::canonicalize(root)
                                            .is_ok_and(|canonical| canonical == *root)
                                    });
                                if !valid_append {
                                    let _ = config
                                        .workspace_roots
                                        .abort_generation(generation.generation)
                                        .await;
                                    let _ = respond.send(Err(
                                        AgentLoopError::InvalidConfiguration(
                                            "workspace root controller returned a non-canonical or non-append generation"
                                                .to_owned(),
                                        ),
                                    ));
                                    return;
                                }
                                let replacement_context =
                                    match ToolContext::from_workspace_roots(&generation.roots) {
                                        Ok(context) => context
                                            .with_session_id(config.session_id.clone())
                                            .with_mcp_tool_policy(
                                                config.tools.mcp_tool_policy().clone(),
                                            ),
                                        Err(_error) => {
                                            let _ = config
                                                .workspace_roots
                                                .abort_generation(generation.generation)
                                                .await;
                                            let _ = respond.send(Err(AgentLoopError::ToolContext(
                                                "workspace tool context could not prepare"
                                                    .to_owned(),
                                            )));
                                            return;
                                        }
                                    };
                                let descriptors = generation
                                    .roots
                                    .iter()
                                    .enumerate()
                                    .map(|(index, _root)| rw_types::WorkspaceRootDescriptor {
                                        index: u32::try_from(index).unwrap_or(u32::MAX),
                                        path: format!("@root/{index}"),
                                        machine_local: false,
                                    })
                                    .collect::<Vec<_>>();
                                if let Err(_error) = config
                                    .workspace_roots
                                    .prepare_commit_generation(generation.generation)
                                    .await
                                {
                                    let _ = config
                                        .workspace_roots
                                        .abort_generation(generation.generation)
                                        .await;
                                    let _ = respond.send(Err(AgentLoopError::Persistence(
                                        "workspace root generation could not commit".to_owned(),
                                    )));
                                    return;
                                }
                                if let Err(_error) = emit(
                                    state,
                                    events,
                                    &config.event_sink,
                                    PendingEvent::WorkspaceRootsChanged {
                                        generation: generation.generation,
                                        effective_from_turn: generation.effective_from_turn,
                                        roots: descriptors,
                                    },
                                )
                                .await
                                {
                                    let _ = config
                                        .workspace_roots
                                        .abort_generation(generation.generation)
                                        .await;
                                    let _ = respond.send(Err(AgentLoopError::Persistence(
                                        "workspace root change event could not persist".to_owned(),
                                    )));
                                    return;
                                }
                                config
                                    .workspace_roots
                                    .finalize_generation(generation.generation);
                                let base_config =
                                    config.with_workspace_generation(&generation, &state.mode_id);
                                let (rebased, development_detached) = config
                                    .extension_development
                                    .rebase(SessionExtensionSnapshot {
                                        revision: base_config.workspace_generation,
                                        workspace_roots: Arc::from(generation.roots.clone()),
                                        tools: Arc::clone(&base_config.tools),
                                        hooks: Arc::clone(&base_config.hooks),
                                        commands: Arc::clone(&base_config.commands),
                                    })
                                    .await;
                                let next_config =
                                    Arc::new(base_config.with_extension_snapshot(&rebased));
                                *command_descriptors
                                    .write()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::from(
                                    next_config
                                        .commands
                                        .descriptors()
                                        .cloned()
                                        .collect::<Vec<_>>(),
                                );
                                *mode_registry
                                    .write()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Arc::clone(&next_config.modes);
                                *config = next_config;
                                *tool_context = replacement_context;
                                output.message = format!(
                                    "added workspace root @root/{}",
                                    generation.roots.len() - 1
                                );
                                if development_detached {
                                    output.message.push_str(
                                        "; detached the development plugin after a registry collision",
                                    );
                                }
                            }
                            SessionCommandAction::Trust { operation } => {
                                match config.folder_trust.execute(operation).await {
                                    Ok(message) => output.message = message,
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::InitializeWorkspace { depth } => {
                                if state.running.is_some()
                                    || state.initialization_running
                                    || config.tools.session_activity(&state.session_id).is_some()
                                {
                                    let _ =
                                        respond.send(Err(AgentLoopError::InvalidConfiguration(
                                            "workspace initialization requires an idle session"
                                                .to_owned(),
                                        )));
                                    return;
                                }
                                let call_id = format!(
                                    "command-init-{}-{}",
                                    state.next_turn,
                                    state
                                        .sequence
                                        .map_or(0, |sequence| sequence.saturating_add(1))
                                );
                                state.initialization_running = true;
                                start_workspace_initialization(
                                    config.workspace_root.clone(),
                                    depth,
                                    config.session_id.clone(),
                                    state.next_turn,
                                    call_id,
                                    Arc::clone(&config.checkpoints),
                                    turn_signals.clone(),
                                );
                                deferred_command_completion = true;
                            }
                            SessionCommandAction::SubmitPrompt {
                                content,
                                model_alias,
                                allowed_tools,
                                permission_patterns,
                                tool_calls,
                            } => {
                                if state.running.is_some() {
                                    let _ =
                                        respond.send(Err(AgentLoopError::InvalidConfiguration(
                                            "custom commands require an idle session".to_owned(),
                                        )));
                                    return;
                                }
                                submitted_prompt = Some((
                                    content,
                                    CommandTurnOverrides {
                                        model_alias,
                                        allowed_tools,
                                        permission_patterns,
                                        tool_calls,
                                    },
                                ));
                            }
                            SessionCommandAction::None => {}
                        }
                        if deferred_command_completion {
                            let _ = respond.send(Ok(MessageDisposition::Command));
                            return;
                        }
                        let name = content
                            .trim_start()
                            .trim_start_matches('/')
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_owned();
                        let persisted = emit(
                            state,
                            events,
                            &config.event_sink,
                            PendingEvent::CommandFinished {
                                name,
                                message: output.message,
                                unrestorable_paths,
                            },
                        )
                        .await;
                        match (persisted, submitted_prompt) {
                            (Err(error), _) => Err(error),
                            (Ok(()), None) => Ok(MessageDisposition::Command),
                            (Ok(()), Some((prompt, overrides))) => start_turn_with_overrides(
                                state,
                                StartTurnRuntime {
                                    config,
                                    tool_context,
                                    signals: turn_signals,
                                    events,
                                    active_turn,
                                },
                                vec![(prompt, Vec::new())],
                                overrides,
                            )
                            .await
                            .map(|()| MessageDisposition::Started),
                        }
                    }
                    Err(error) => {
                        let persisted = emit(
                            state,
                            events,
                            &config.event_sink,
                            PendingEvent::Error {
                                message: error.to_string(),
                            },
                        )
                        .await;
                        Err(persisted
                            .err()
                            .unwrap_or_else(|| AgentLoopError::Extension(error.to_string())))
                    }
                };
                let _ = respond.send(disposition);
            } else if state.initialization_running {
                let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                    "workspace initialization is still running".to_owned(),
                )));
            } else if state.running.is_some() {
                let content = config.secret_redactor.redact(&content);
                let Some(position) = state
                    .queued_positions
                    .back()
                    .copied()
                    .unwrap_or(0)
                    .checked_add(1)
                else {
                    let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                        "queued message position space is exhausted".to_owned(),
                    )));
                    return;
                };
                state.queued.push_back(content.clone());
                state.queued_positions.push_back(position);
                let persisted = emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::MessageQueued {
                        position,
                        content,
                        attachments: Vec::new(),
                    },
                )
                .await;
                if let Err(error) = persisted {
                    state.queued.pop_back();
                    state.queued_positions.pop_back();
                    let _ = respond.send(Err(error));
                } else {
                    let _ = respond.send(Ok(MessageDisposition::Queued));
                }
            } else {
                let result = start_turn(
                    state,
                    config,
                    tool_context,
                    turn_signals,
                    events,
                    vec![(content, attachments)],
                    active_turn,
                )
                .await;
                let _ = respond.send(result.map(|()| MessageDisposition::Started));
            }
        }
        #[cfg(test)]
        ActorCommand::Interrupt {
            target_turn,
            respond,
        } => {
            let interrupted = state.running.as_ref().is_some_and(|running| {
                if running.id != target_turn {
                    return false;
                }
                running.cancellation.cancel();
                true
            });
            let _ = respond.send(interrupted);
        }
        ActorCommand::CompleteUserShell {
            shell_id,
            status,
            captured_output,
            respond,
        } => {
            let captured_output =
                captured_output.map(|output| config.secret_redactor.redact(&output));
            let result = if captured_output
                .as_ref()
                .is_some_and(|output| output.len() > MAX_CAPTURED_SHELL_OUTPUT_BYTES)
            {
                Err(AgentLoopError::InvalidConfiguration(
                    "captured foreground-shell output exceeds the durable limit".to_owned(),
                ))
            } else if state
                .active_shell
                .as_ref()
                .is_none_or(|active| active.shell_id != shell_id)
            {
                Err(AgentLoopError::InvalidConfiguration(
                    "foreground-shell completion does not match the active shell id".to_owned(),
                ))
            } else {
                let command = state
                    .active_shell
                    .as_ref()
                    .map(|active| active.command.clone())
                    .unwrap_or_default();
                let context = shell_context_turn(&command, status, captured_output.as_deref());
                let persisted = emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::UserShellStateChanged {
                        shell_id,
                        command,
                        active: false,
                        status: Some(status),
                        captured_output,
                    },
                )
                .await;
                if persisted.is_ok() {
                    state.conversation.push(context);
                    state.active_shell = None;
                }
                persisted
            };
            let _ = respond.send(result);
        }
        ActorCommand::RecordSubagentSpawned {
            subagent_id,
            child_session_id,
            task,
            respond,
        } => {
            let result = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::SubagentSpawned {
                    subagent_id,
                    child_session_id,
                    task,
                },
            )
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::RecordSubagentFinished { result, respond } => {
            let subagent_id = result.subagent_id.clone();
            let result = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::SubagentFinished {
                    subagent_id,
                    result,
                },
            )
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::PublishSubagentProgressBatch { progress, respond } => {
            for progress in progress {
                let _ = events.send(RoutedEvent {
                    target: None,
                    event: EngineEvent::SubagentProgress {
                        parent_session_id: state.session_id.clone(),
                        subagent_id: progress.subagent_id,
                        child_session_id: progress.child_session_id,
                        child_sequence: progress.child_sequence.map(SequenceId),
                        event: progress.event,
                    },
                });
            }
            let _ = respond.send(Ok(()));
        }
        ActorCommand::Snapshot { respond } => {
            let _ = respond.send(SessionSnapshot {
                conversation: state.conversation.clone(),
                queued_messages: state.queued.iter().cloned().collect(),
                running: state.running.is_some(),
                completed_turns: state.completed_turns,
                model_alias: state.model_alias.clone(),
                provider: state.provider.clone(),
                thinking: state.thinking,
                mode: state.mode,
                mode_id: state.mode_id.clone(),
                permission_mode: config.permissions.snapshot().runtime_mode,
                pending_plan: state.pending_plan.clone(),
                approved_plan: state.approved_plan.clone(),
                plan_gate_active: state.plan_gate_active,
                active_shell: state.active_shell.clone(),
                active_background: config.tools.session_activity(&state.session_id).is_some(),
                workspace_generation: config.workspace_generation,
                workspace_roots: std::iter::once(&config.workspace_root)
                    .chain(&config.additional_workspace_roots)
                    .enumerate()
                    .map(|(index, _root)| rw_types::WorkspaceRootDescriptor {
                        index: u32::try_from(index).unwrap_or(u32::MAX),
                        path: format!("@root/{index}"),
                        machine_local: false,
                    })
                    .collect(),
                driver_client_id: state.driver_client_id.clone(),
            });
        }
    }
}

async fn rewind_state(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
    to_turn: u64,
) -> Result<Vec<UnrestorablePath>, AgentLoopError> {
    if state.running.is_some() {
        return Err(AgentLoopError::InvalidConfiguration(
            "cannot rewind while a turn is running".to_owned(),
        ));
    }
    if let Some((pending_turn, pending)) = state.pending_rewind.clone() {
        if pending_turn != to_turn {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "rewind to turn {pending_turn} is awaiting acknowledgement"
            )));
        }
        if let Err(error) = config.checkpoints.acknowledge_rewind(&pending).await {
            state.poisoned = true;
            return Err(error);
        }
        state.pending_rewind = None;
        state.poisoned = false;
        return Ok(pending.unrestorable_paths);
    }
    let Some(&conversation_len) = state.turn_ends.get(&to_turn) else {
        return Err(AgentLoopError::InvalidConfiguration(format!(
            "turn {to_turn} is not a completed rewind target"
        )));
    };
    let historical = config
        .event_sink
        .read_after(None)
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    let historical = historical
        .iter()
        .rposition(|event| {
            matches!(event, EngineEvent::TurnFinished { turn_id, .. } if parse_turn_id(turn_id) == Ok(to_turn))
        })
        .map(|boundary| {
            project_session_events_with_modes(&historical[..=boundary], &config.modes)
        })
        .transpose()
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    let operation_id = format!(
        "rewind-{}-{to_turn}",
        state
            .sequence
            .map_or(0, |sequence| sequence.saturating_add(1))
    );
    let rewind = config
        .checkpoints
        .prepare_apply_rewind(&config.session_id, to_turn, &operation_id)
        .await?;
    if let Err(error) = emit(
        state,
        events,
        &config.event_sink,
        PendingEvent::ConversationRewound {
            to_turn,
            operation_id,
            unrestorable_paths: rewind.unrestorable_paths.clone(),
        },
    )
    .await
    {
        state.poisoned = true;
        return Err(error);
    }
    if let Some(historical) = historical {
        state.conversation = historical.conversation;
        state.context_surgery = historical.context_surgery;
        state.pruned_tool_outputs = historical.pruned_tool_outputs;
        state.budgeter = historical.budgeter;
        state.mode = historical.mode;
        state.mode_id = historical
            .mode_id
            .unwrap_or_else(|| ModeId(session_mode_name(historical.mode).to_owned()));
        state.pending_plan = historical.pending_plan;
        state.approved_plan = historical.approved_plan;
        state.plan_gate_active = historical.plan_gate_active;
    } else {
        state.conversation.truncate(conversation_len);
        state
            .context_surgery
            .retain(|action| action.effective_after_agent_turn <= to_turn);
    }
    state.turn_ends.retain(|turn, _| *turn <= to_turn);
    state.completed_turns = u64::try_from(state.turn_ends.len()).unwrap_or(u64::MAX);
    state.queued.clear();
    state.queued_positions.clear();
    state.pending_rewind = Some((to_turn, rewind.clone()));
    if let Err(error) = config.checkpoints.acknowledge_rewind(&rewind).await {
        state.poisoned = true;
        return Err(error);
    }
    state.pending_rewind = None;
    Ok(rewind.unrestorable_paths)
}
