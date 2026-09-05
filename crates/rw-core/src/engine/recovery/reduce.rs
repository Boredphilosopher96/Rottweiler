use super::{
    RecoveryError,
    encoding::serialized_size,
    projector::{BatchRows, key},
    state::{
        ACCOUNTING, ACTIVE_ASSISTANT, ACTIVE_TOOL_LIFECYCLE, ACTIVE_TOOL_RESULTS, AcceptedSource,
        ActiveSource, ActiveTurn, BOUNDARIES, Boundary, CONTEXT_ACTIONS, CONVERSATION,
        ConversationCut, ConversationSource, MAX_QUESTIONS, MAX_QUEUED, Maintenance,
        PRUNED_OUTPUTS, QuestionSource, QueuedSource, RecoveryHead, RewindPhase, SOURCE_ORDINAL,
        SourceTotals, ToolLifecycleSource, ToolStartIdentity, TurnSourceKind,
    },
};
use crate::engine::{
    PendingEvent, SESSION_EVENT_VERSION, mode_permission_base,
    projection::{recovered_pending_event, shell_context_turn},
};
use rw_context::{Budgeter, LocalTokenEstimator};
use rw_ext::ModeRegistry;
use rw_providers::TokenUsage;
use rw_types::{EngineEvent, Role, SequenceId, SessionMode, Turn};

#[expect(
    clippy::too_many_lines,
    reason = "Keep the canonical durable transition match exhaustive in one place."
)]
pub(super) fn reduce(
    head: &mut RecoveryHead,
    event: &EngineEvent,
    modes: &ModeRegistry,
    rows: &mut BatchRows,
) -> Result<(), RecoveryError> {
    let meta = event
        .meta()
        .ok_or(RecoveryError::Invalid("non-durable event"))?;
    if meta.protocol_version != SESSION_EVENT_VERSION || meta.sequence_id.0 != head.next_sequence {
        return Err(RecoveryError::Invalid("durable event version/sequence"));
    }
    if head
        .session_id
        .as_ref()
        .is_some_and(|id| id != &meta.session_id)
    {
        return Err(RecoveryError::Invalid("session identity changed"));
    }
    head.session_id = Some(meta.session_id.clone());
    let sequence = meta.sequence_id;
    let Some(kind) = recovered_pending_event(event)? else {
        head.next_sequence += 1;
        return Ok(());
    };
    match kind {
        PendingEvent::TodoStateCommitted { snapshot } => {
            snapshot
                .validate()
                .map_err(|_| RecoveryError::Invalid("task snapshot"))?;
            head.control.todos = Some(sequence);
        }
        PendingEvent::ConversationTurnCommitted { agent_turn, turn } => {
            append_turn(
                head,
                rows,
                sequence,
                agent_turn,
                TurnSourceKind::Committed,
                &turn,
            )?;
            if head.compacting.is_none() {
                if turn.role == Role::User
                    && let Some(index) = head
                        .control
                        .accepted
                        .iter()
                        .position(|accepted| accepted.agent_turn == agent_turn)
                {
                    head.control.accepted.remove(index);
                }
                if let Some(active) = &mut head.control.active {
                    match turn.role {
                        Role::Assistant => {
                            active.last_assistant_commit = Some(sequence);
                            active.assistant_parts = SourceTotals::default();
                        }
                        Role::Tool => {
                            active.last_tool_commit = Some(sequence);
                            active.tool_results = SourceTotals::default();
                        }
                        _ => {}
                    }
                }
            }
        }
        PendingEvent::ConversationRewound { to_turn, .. } => {
            let boundary: Boundary = rows
                .get(key(BOUNDARIES, 0, to_turn))?
                .ok_or(RecoveryError::Invalid("unknown rewind boundary"))?;
            head.conversation = boundary.conversation;
            head.control.completed_turns = boundary.control.completed_turns;
            head.control.todos = boundary.control.todos;
            head.control.mode = boundary.control.mode;
            head.control.mode_id = boundary.control.mode_id;
            head.control.pending_plan = boundary.control.pending_plan;
            head.control.approved_plan = boundary.control.approved_plan;
            head.control.plan_gate_active = boundary.control.plan_gate_active;
            head.control.queued.clear();
            head.control
                .accepted
                .retain(|accepted| accepted.agent_turn <= to_turn);
            head.control
                .questions
                .retain(|question| question.agent_turn <= to_turn);
            head.control.active = None;
            head.context_cut = boundary.context_cut;
            head.budget = boundary.budget;
            head.extension_root = boundary.extension_root;
            head.compacting = None;
            head.maintenance = Some(Maintenance::Rewind {
                sequence,
                target: to_turn,
                phase: RewindPhase::Boundaries,
            });
            return Ok(());
        }
        PendingEvent::TurnStarted { turn } => {
            head.control.active = Some(ActiveTurn {
                turn,
                started: sequence,
                first_conversation_ordinal: head.conversation.turns,
                last_assistant_commit: None,
                last_tool_commit: None,
                assistant_parts: SourceTotals::default(),
                tool_lifecycle: SourceTotals::default(),
                tool_results: SourceTotals::default(),
            });
            head.control.next_turn = head.control.next_turn.max(turn.saturating_add(1));
        }
        PendingEvent::TurnFinished { turn, .. } => {
            if head
                .control
                .active
                .as_ref()
                .is_some_and(|active| active.turn == turn)
            {
                head.control.active = None;
            }
            head.control.completed_turns = head.control.completed_turns.saturating_add(1);
            head.control.next_turn = head.control.next_turn.max(turn.saturating_add(1));
            head.control
                .questions
                .retain(|question| question.agent_turn != turn);
            rows.put(
                key(BOUNDARIES, 0, turn),
                &Boundary {
                    source_sequence: sequence,
                    conversation: head.conversation,
                    control: head.control.clone(),
                    context_cut: head.context_cut,
                    budget: head.budget,
                    extension_root: head.extension_root,
                },
            )?;
            rows.put(key(ACCOUNTING, 0, sequence.0), &sequence)?;
        }
        PendingEvent::MessageQueued {
            position, content, ..
        } => {
            if head.control.queued.len() >= MAX_QUEUED {
                return Err(RecoveryError::Limit("queued message identities"));
            }
            head.control.queued.push(QueuedSource {
                position,
                sequence,
                content_digest: *blake3::hash(content.as_bytes()).as_bytes(),
            });
        }
        PendingEvent::QueuedMessageRemoved { position } => {
            if let Some(index) = head
                .control
                .queued
                .iter()
                .position(|queued| queued.position == position)
            {
                head.control.queued.remove(index);
            }
        }
        PendingEvent::QueuedMessagesCleared => head.control.queued.clear(),
        PendingEvent::UserMessageAccepted { turn, content, .. } => {
            let digest = *blake3::hash(content.as_bytes()).as_bytes();
            if let Some(index) = head
                .control
                .queued
                .iter()
                .position(|queued| queued.content_digest == digest)
            {
                head.control.queued.remove(index);
            }
            if head.control.accepted.len() >= MAX_QUEUED {
                return Err(RecoveryError::Limit("accepted message identities"));
            }
            head.control.accepted.push(AcceptedSource {
                agent_turn: turn,
                sequence,
            });
        }
        PendingEvent::QuestionAsked {
            turn, question_id, ..
        } => {
            head.control
                .questions
                .retain(|question| question.id != question_id.0);
            if head.control.questions.len() >= MAX_QUESTIONS {
                return Err(RecoveryError::Limit("pending question identities"));
            }
            head.control.questions.push(QuestionSource {
                id: question_id.0,
                agent_turn: turn,
                sequence,
            });
        }
        PendingEvent::QuestionAnswered { question_id, .. } => head
            .control
            .questions
            .retain(|question| question.id != question_id.0),
        PendingEvent::SessionTitleUpdated { usage, cost, .. } => {
            head.control.title = Some(sequence);
            if usage.is_some() && cost.is_some() {
                rows.put(key(ACCOUNTING, 0, sequence.0), &sequence)?;
            }
        }
        PendingEvent::SessionCreated { driver_client_id }
        | PendingEvent::DriverChanged { driver_client_id } => {
            head.control.driver = Some(driver_client_id);
        }
        PendingEvent::ModelChanged { .. } => head.control.model = Some(sequence),
        PendingEvent::ModeChanged {
            mode,
            definition_fingerprint,
        } => {
            let definition = modes.get(&mode.0).ok_or_else(|| {
                crate::engine::SessionProjectionError::InvalidMode(mode.0.clone())
            })?;
            if definition.semantic_fingerprint() != definition_fingerprint {
                return Err(
                    crate::engine::SessionProjectionError::ModeDefinitionChanged(mode.0).into(),
                );
            }
            head.control.mode = mode_permission_base(definition);
            head.control.mode_id = Some(mode);
            if head.control.mode == SessionMode::Plan {
                head.control.pending_plan = None;
                head.control.approved_plan = None;
                head.control.plan_gate_active = true;
            }
        }
        PendingEvent::PermissionModeChanged { .. } => head.control.permission_mode = Some(sequence),
        PendingEvent::PlanSubmitted { .. } => head.control.pending_plan = Some(sequence),
        PendingEvent::PlanReviewed { decision, .. } => {
            head.control.pending_plan = None;
            if decision == rw_types::PlanDecision::Approve {
                head.control.approved_plan = Some(sequence);
                head.control.plan_gate_active = false;
            }
        }
        PendingEvent::WorkspaceRootsChanged {
            generation, roots, ..
        } => {
            super::workspace::apply_workspace_generation(
                &mut head.control,
                sequence,
                generation,
                &roots,
            )?;
        }
        PendingEvent::UserShellStateChanged {
            shell_id,
            command,
            active: true,
            status: None,
            captured_output: None,
        } => {
            if head.control.active_shell.is_some() {
                return Err(RecoveryError::Invalid("overlapping foreground shells"));
            }
            let _ = command;
            head.control.active_shell = Some((shell_id.0, sequence));
        }
        PendingEvent::UserShellStateChanged {
            shell_id,
            command,
            active: false,
            status: Some(status),
            captured_output,
        } => {
            if head
                .control
                .active_shell
                .as_ref()
                .is_none_or(|(id, _)| id != &shell_id.0)
            {
                return Err(RecoveryError::Invalid("foreground shell terminal identity"));
            }
            let turn = shell_context_turn(&command, status, captured_output.as_deref());
            append_turn(
                head,
                rows,
                sequence,
                head.control.next_turn.saturating_sub(1),
                TurnSourceKind::Shell,
                &turn,
            )?;
            head.control.active_shell = None;
        }
        PendingEvent::UserShellStateChanged { .. } => {
            return Err(RecoveryError::Invalid("foreground shell transition"));
        }
        PendingEvent::CompactionStarted { .. } => {
            head.compacting = Some(ConversationCut {
                generation: sequence.0.saturating_add(1),
                ..ConversationCut::default()
            });
        }
        PendingEvent::CompactionFinished { usage, cost, .. } => {
            if let Some(cut) = head.compacting.take() {
                head.conversation = cut;
                if let Some(active) = &mut head.control.active {
                    active.replace_conversation(sequence);
                }
            }
            if usage.is_some() && cost.is_some() {
                rows.put(key(ACCOUNTING, 0, sequence.0), &sequence)?;
            }
        }
        PendingEvent::ProviderCallAccounted { call, actuals } => {
            super::receipts::index(head, meta, call, actuals, rows)?;
            rows.put(key(ACCOUNTING, 0, sequence.0), &sequence)?;
        }
        PendingEvent::CompactionAttemptFinished { .. } => {
            rows.put(key(ACCOUNTING, 0, sequence.0), &sequence)?;
        }
        PendingEvent::Error { .. } | PendingEvent::CompactionFailed { .. } => {
            head.compacting = None;
        }
        PendingEvent::ModelContextCleared { .. } => {
            head.maintenance = Some(Maintenance::Clear {
                sequence,
                from: head.conversation,
                after: None,
                to: ConversationCut {
                    generation: sequence.0.saturating_add(1),
                    ..ConversationCut::default()
                },
            });
            return Ok(());
        }
        PendingEvent::ContextItemPinned {
            item_id,
            effective_after_agent_turn,
        } => context_change(
            head,
            rows,
            sequence,
            &item_id,
            true,
            effective_after_agent_turn,
        )?,
        PendingEvent::ContextItemEvicted {
            item_id,
            effective_after_agent_turn,
        } => context_change(
            head,
            rows,
            sequence,
            &item_id,
            false,
            effective_after_agent_turn,
        )?,
        PendingEvent::ToolOutputPruned {
            tool_call_id,
            reclaimed_tokens,
        } => {
            rows.put(
                key(PRUNED_OUTPUTS, head.conversation.generation, sequence.0),
                &(tool_call_id, reclaimed_tokens),
            )?;
            head.context_cut = sequence.0;
        }
        PendingEvent::ContextUsage {
            estimated_input_tokens,
            provider_input_tokens,
            ..
        } => {
            let mut budget = Budgeter::from_snapshot(head.budget)
                .map_err(|_| RecoveryError::Invalid("budget reconciliation"))?;
            budget.reconcile(
                estimated_input_tokens,
                TokenUsage {
                    input_tokens: provider_input_tokens,
                    ..TokenUsage::default()
                },
            );
            head.budget = budget.snapshot();
        }
        PendingEvent::TextDelta { turn, .. }
        | PendingEvent::ThinkingDelta { turn, .. }
        | PendingEvent::CitationDelta { turn, .. } => {
            active_source(head, rows, event, turn, ACTIVE_ASSISTANT)?;
        }
        PendingEvent::ToolCallStarted {
            turn,
            id,
            invocation_id,
            index,
            ..
        } => {
            tool_lifecycle(
                head,
                rows,
                turn,
                sequence,
                &ToolLifecycleSource::Started(ToolStartIdentity {
                    invocation_id,
                    tool_call_id: rw_types::ToolCallId(id),
                    index,
                }),
            )?;
        }
        PendingEvent::ToolCallFinished {
            turn,
            invocation_id,
            ..
        } => {
            tool_lifecycle(
                head,
                rows,
                turn,
                sequence,
                &ToolLifecycleSource::Finished(invocation_id),
            )?;
            active_source(head, rows, event, turn, ACTIVE_TOOL_RESULTS)?;
        }
        PendingEvent::ExtensionStateCommitted {
            plugin_id,
            transaction,
        } => {
            super::extension::apply(head, rows, sequence, &plugin_id, &transaction)?;
        }
        PendingEvent::ToolOutput { .. }
        | PendingEvent::PermissionRequested { .. }
        | PendingEvent::ToolDiffReady { .. }
        | PendingEvent::HookFailure { .. }
        | PendingEvent::CommandFinished { .. }
        | PendingEvent::GuardTriggered { .. }
        | PendingEvent::BudgetStatus { .. }
        | PendingEvent::SubagentSpawned { .. }
        | PendingEvent::SubagentFinished { .. }
        | PendingEvent::PluginMessageInjected { .. }
        | PendingEvent::PluginStatusChanged { .. }
        | PendingEvent::UiNotification { .. } => {}
    }
    head.next_sequence = sequence
        .0
        .checked_add(1)
        .ok_or(RecoveryError::Invalid("sequence overflow"))?;
    head.validate()?;
    Ok(())
}

fn append_turn(
    head: &mut RecoveryHead,
    rows: &mut BatchRows,
    sequence: SequenceId,
    agent_turn: u64,
    kind: TurnSourceKind,
    turn: &Turn,
) -> Result<(), RecoveryError> {
    let cut = head.compacting.as_mut().unwrap_or(&mut head.conversation);
    let bytes = serialized_size(turn)?;
    let tokens = LocalTokenEstimator::turn(turn);
    cut.serialized_bytes = cut
        .serialized_bytes
        .checked_add(bytes)
        .ok_or(RecoveryError::Limit("conversation byte counter"))?;
    cut.estimated_tokens = cut
        .estimated_tokens
        .checked_add(tokens)
        .ok_or(RecoveryError::Limit("conversation token counter"))?;
    rows.put(
        key(CONVERSATION, cut.generation, cut.turns),
        &ConversationSource {
            sequence,
            kind,
            agent_turn,
            role: turn.role.clone(),
            serialized_bytes: bytes,
            estimated_tokens: tokens,
            cumulative_bytes: cut.serialized_bytes,
            cumulative_tokens: cut.estimated_tokens,
        },
    )?;
    rows.put(key(SOURCE_ORDINAL, cut.generation, sequence.0), &cut.turns)?;
    cut.turns = cut
        .turns
        .checked_add(1)
        .ok_or(RecoveryError::Limit("conversation ordinal"))?;
    Ok(())
}
fn context_change(
    head: &mut RecoveryHead,
    rows: &mut BatchRows,
    sequence: SequenceId,
    item: &rw_types::ContextItemId,
    pinned: bool,
    effective: u64,
) -> Result<(), RecoveryError> {
    let generation = head
        .compacting
        .as_ref()
        .unwrap_or(&head.conversation)
        .generation;
    rows.put(
        key(CONTEXT_ACTIONS, generation, sequence.0),
        &(item, pinned, effective),
    )?;
    head.context_cut = sequence.0;
    Ok(())
}

fn active_source(
    head: &mut RecoveryHead,
    rows: &mut BatchRows,
    event: &EngineEvent,
    turn: u64,
    namespace: u8,
) -> Result<(), RecoveryError> {
    let Some(active) = head
        .control
        .active
        .as_mut()
        .filter(|active| active.turn == turn)
    else {
        return Ok(());
    };
    let totals = match namespace {
        ACTIVE_ASSISTANT => &mut active.assistant_parts,
        ACTIVE_TOOL_RESULTS => &mut active.tool_results,
        _ => return Err(RecoveryError::Invalid("active source namespace")),
    };
    let source = ActiveSource {
        sequence: event
            .meta()
            .ok_or(RecoveryError::Invalid("non-durable active source"))?
            .sequence_id,
        serialized_bytes: serialized_size(event)?,
    };
    totals.records = totals
        .records
        .checked_add(1)
        .ok_or(RecoveryError::Limit("active source count"))?;
    totals.serialized_bytes = totals
        .serialized_bytes
        .checked_add(source.serialized_bytes)
        .ok_or(RecoveryError::Limit("active source bytes"))?;
    rows.put(key(namespace, turn, source.sequence.0), &source)
}

fn tool_lifecycle(
    head: &mut RecoveryHead,
    rows: &mut BatchRows,
    turn: u64,
    sequence: SequenceId,
    source: &ToolLifecycleSource,
) -> Result<(), RecoveryError> {
    let Some(active) = head
        .control
        .active
        .as_mut()
        .filter(|active| active.turn == turn)
    else {
        return Ok(());
    };
    active.tool_lifecycle.records = active
        .tool_lifecycle
        .records
        .checked_add(1)
        .ok_or(RecoveryError::Limit("tool lifecycle count"))?;
    active.tool_lifecycle.serialized_bytes = active
        .tool_lifecycle
        .serialized_bytes
        .checked_add(serialized_size(source)?)
        .ok_or(RecoveryError::Limit("tool lifecycle bytes"))?;
    rows.put(key(ACTIVE_TOOL_LIFECYCLE, turn, sequence.0), source)
}
