use super::{
    RecoveryError,
    encoding::serialized_size,
    projector::{BatchRows, key},
    state::{
        ACCOUNTING, ACTIVE_ASSISTANT, ACTIVE_TOOL_LIFECYCLE, ACTIVE_TOOL_RESULTS, ActiveSource,
        ActiveTurn, BOUNDARIES, Boundary, CONVERSATION, ConversationCut, ConversationSource,
        MAX_QUESTIONS, MAX_QUEUED, Maintenance, QuestionSource, QueuedSource, RecoveryHead,
        RewindPhase, SOURCE_ORDINAL, SourceTotals, ToolLifecycleSource, ToolStartIdentity,
        TurnSourceKind,
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
    source: &rw_store::session::journal::JournalReadView,
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
    let checked = head
        .control
        .input_claims
        .advance(event)
        .map_err(RecoveryError::Invalid)?;
    let body_source = super::context_selection::validate(head, event, rows)?;
    let materialized = super::input::materialize_claimed_event(source, checked)?;
    let Some(kind) = recovered_pending_event(&materialized)? else {
        head.next_sequence += 1;
        return Ok(());
    };
    match kind {
        PendingEvent::ConversationToolResultsCommitted { .. }
        | PendingEvent::ConversationInputCommitted { .. }
        | PendingEvent::ConversationContextCommitted { .. } => {
            return Err(RecoveryError::Invalid("unresolved input commit"));
        }
        PendingEvent::BudgetStatus { .. } => head.latest_budget = Some(sequence),
        PendingEvent::TodoStateCommitted { snapshot } => {
            snapshot
                .validate()
                .map_err(|_| RecoveryError::Invalid("task snapshot"))?;
            head.control.todos = Some(sequence);
        }
        PendingEvent::ConversationTurnCommitted { agent_turn, turn } => {
            let mut citation_admission = head
                .control
                .active
                .as_ref()
                .filter(|active| active.turn == agent_turn && head.compacting.is_none())
                .map_or_else(Default::default, |active| active.committed_citations);
            for block in &turn.blocks {
                if let rw_types::Block::Citation {
                    uri,
                    title,
                    excerpt,
                } = block
                {
                    citation_admission
                        .admit(uri, title.as_ref(), excerpt.as_ref())
                        .map_err(RecoveryError::Limit)?;
                }
            }
            if head.compacting.is_none()
                && let Some(active) = &mut head.control.active
                && active.turn == agent_turn
            {
                active.committed_citations = citation_admission;
            }
            append_turn(
                head,
                rows,
                sequence,
                body_source,
                agent_turn,
                TurnSourceKind::Committed,
                &turn,
            )?;
            if head.compacting.is_none()
                && let Some(active) = &mut head.control.active
            {
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
        PendingEvent::ConversationRewound { to_turn, .. } => {
            let boundary: Boundary = rows
                .get(key(BOUNDARIES, 0, to_turn))?
                .ok_or(RecoveryError::Invalid("unknown rewind boundary"))?;
            head.apply_rewind_boundary(&boundary, to_turn);
            head.maintenance = Some(Maintenance::Rewind {
                sequence,
                target: to_turn,
                phase: RewindPhase::Boundaries,
            });
            return Ok(());
        }
        PendingEvent::TurnStarted { turn } => {
            rows.delete(key(super::state::PROMPTS, 0, turn));
            head.control.active = Some(ActiveTurn {
                announced_citations: rw_types::citation_admission::CitationAdmission::default(),
                committed_citations: rw_types::citation_admission::CitationAdmission::default(),
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
        PendingEvent::TurnFinished {
            turn, usage, cost, ..
        } => {
            head.accounting.record_actuals(&usage.into(), &cost);
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
        PendingEvent::UserMessageAccepted {
            content,
            attachments,
            ..
        } => {
            crate::engine::dispatch::recover_user_message(&content, &attachments)
                .map_err(crate::engine::SessionProjectionError::InvalidAttachment)?;
            let digest = *blake3::hash(content.as_bytes()).as_bytes();
            if let Some(index) = head
                .control
                .queued
                .iter()
                .position(|queued| queued.content_digest == digest)
            {
                head.control.queued.remove(index);
            }
        }
        PendingEvent::QuestionAsked {
            turn,
            question_id,
            question,
        } => {
            if question.id != question_id {
                return Err(RecoveryError::Invalid("question source identity mismatch"));
            }
            rw_types::question_admission::validate_question(&question)
                .map_err(RecoveryError::Limit)?;
            if head
                .control
                .questions
                .iter()
                .any(|pending| pending.id == question_id.0)
            {
                return Err(RecoveryError::Invalid(
                    "pending question identity is already in use",
                ));
            }
            if head.control.questions.len() >= MAX_QUESTIONS {
                return Err(RecoveryError::Limit("pending question identities"));
            }
            head.control.questions.push(QuestionSource {
                id: question_id.0,
                agent_turn: turn,
                sequence,
            });
        }
        PendingEvent::QuestionAnswered {
            turn,
            question_id,
            answer,
        } => {
            super::questions::answer(head, source, turn, &question_id, &answer)?;
        }
        PendingEvent::SessionTitleUpdated { usage, cost, .. } => {
            head.control.title = Some(sequence);
            if let (Some(usage), Some(cost)) = (usage, cost) {
                head.accounting.record_actuals(&usage.into(), &cost);
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
        PendingEvent::PlanSubmitted { artifact } => {
            rw_types::session_controls::validate_plan(&artifact).map_err(RecoveryError::Limit)?;
            head.control.pending_plan = Some(sequence);
        }
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
            if let (Some(usage), Some(cost)) = (usage, cost) {
                head.accounting.record_actuals(&usage.into(), &cost);
                rows.put(key(ACCOUNTING, 0, sequence.0), &sequence)?;
            }
        }
        PendingEvent::ProviderCallAccounted { call, actuals } => {
            super::receipts::index(head, meta, call, actuals, rows)?;
            rows.put(key(ACCOUNTING, 0, sequence.0), &sequence)?;
        }
        PendingEvent::CompactionAttemptFinished { usage, cost, .. } => {
            head.accounting.record_actuals(&usage.into(), &cost);
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
                to: Box::new(ConversationCut {
                    generation: sequence.0.saturating_add(1),
                    ..ConversationCut::default()
                }),
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
            source,
            reclaimed_tokens,
        } => {
            let cut = head.compacting.as_ref().unwrap_or(&head.conversation);
            validate_source(rows, cut, source.sequence)?;
            super::pruning::apply(rows, cut.generation, sequence, source, reclaimed_tokens)?;
            head.context_cut = sequence.0;
        }
        PendingEvent::ContextUsage {
            turn,
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
            if rows
                .get::<RecoveryHead>(key(super::state::PROMPTS, 0, turn))?
                .is_none()
            {
                let mut prompt = head.clone();
                prompt.next_sequence = sequence
                    .0
                    .checked_add(1)
                    .ok_or(RecoveryError::Invalid("prompt sequence overflow"))?;
                rows.put(key(super::state::PROMPTS, 0, turn), &prompt)?;
            }
        }
        PendingEvent::CitationDelta { turn, uri, title } => {
            let active = head
                .control
                .active
                .as_mut()
                .filter(|active| active.turn == turn)
                .ok_or(RecoveryError::Invalid("citation has no active turn"))?;
            active
                .announced_citations
                .admit(&uri, title.as_ref(), None)
                .map_err(RecoveryError::Limit)?;
            active_source(head, rows, event, turn, ACTIVE_ASSISTANT)?;
        }
        PendingEvent::TextDelta { turn, .. } | PendingEvent::ThinkingDelta { turn, .. } => {
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
        PendingEvent::PluginStatusChanged { plugin_id, status } => {
            rw_types::session_state::validate_plugin_status(&plugin_id, &status)
                .map_err(RecoveryError::Invalid)?;
            if status.is_empty() {
                head.plugin_statuses.remove(&plugin_id);
            } else {
                if !head.plugin_statuses.contains_key(&plugin_id)
                    && head.plugin_statuses.len()
                        >= rw_types::session_state::MAX_SESSION_PLUGIN_STATUSES
                {
                    return Err(RecoveryError::Limit("active plugin statuses"));
                }
                head.plugin_statuses.insert(plugin_id, sequence);
            }
        }
        PendingEvent::UserMessageRetained { .. }
        | PendingEvent::ToolApprovalResolved { .. }
        | PendingEvent::ToolOutput { .. }
        | PendingEvent::PermissionRequested { .. }
        | PendingEvent::ToolDiffReady { .. }
        | PendingEvent::HookFailure { .. }
        | PendingEvent::CommandFinished { .. }
        | PendingEvent::GuardTriggered { .. }
        | PendingEvent::SubagentSpawned { .. }
        | PendingEvent::SubagentFinished { .. }
        | PendingEvent::PluginMessageInjected { .. }
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
    body_source: SequenceId,
    agent_turn: u64,
    kind: TurnSourceKind,
    turn: &Turn,
) -> Result<(), RecoveryError> {
    let cut = head.compacting.as_mut().unwrap_or(&mut head.conversation);
    let has_resolved_model = turn
        .meta
        .model
        .as_ref()
        .is_some_and(|model| model.contains('/'));
    if has_resolved_model {
        cut.resolved_model_source = Some(sequence);
    }
    if turn.role == rw_types::Role::System {
        cut.system_turns = cut
            .system_turns
            .checked_add(1)
            .ok_or(RecoveryError::Limit("system turn counter"))?;
        if has_resolved_model {
            cut.system_model_source = Some(sequence);
        }
    }
    let turns = std::slice::from_ref(turn);
    if cut.first_user_source.is_none()
        && crate::engine::turn::title::first_meaningful_user_prompt(turns).is_some()
    {
        cut.first_user_source = Some(sequence);
    }
    cut.has_assistant_text |= crate::engine::turn::title::has_successful_assistant_text(turns);
    if crate::engine::projection::approved_plan_context_item(turns).is_some() {
        cut.approved_plan_ordinal = Some(cut.turns);
    }
    let bytes = serialized_size(turn)?;
    let decoded_bytes = super::encoding::turn_decode_bytes(turn)?;
    let tokens = LocalTokenEstimator::turn(turn);
    cut.serialized_bytes = cut
        .serialized_bytes
        .checked_add(bytes)
        .ok_or(RecoveryError::Limit("conversation byte counter"))?;
    cut.decoded_bytes = cut
        .decoded_bytes
        .checked_add(decoded_bytes)
        .ok_or(RecoveryError::Limit("conversation decoded byte counter"))?;
    cut.estimated_tokens = cut
        .estimated_tokens
        .checked_add(tokens)
        .ok_or(RecoveryError::Limit("conversation token counter"))?;
    rows.put(
        key(CONVERSATION, cut.generation, cut.turns),
        &ConversationSource {
            has_resolved_model,
            sequence,
            body_source,
            kind,
            agent_turn,
            role: turn.role.clone(),
            serialized_bytes: bytes,
            decoded_bytes,
            estimated_tokens: tokens,
            cumulative_bytes: cut.serialized_bytes,
            cumulative_decoded_bytes: cut.decoded_bytes,
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
    let cut = head.compacting.as_ref().unwrap_or(&head.conversation);
    let source = item
        .0
        .strip_prefix("conversation:")
        .and_then(|value| value.parse::<u64>().ok())
        .map(SequenceId)
        .ok_or(RecoveryError::Invalid("context item source"))?;
    if rw_types::context_source::conversation_item(source) != *item {
        return Err(RecoveryError::Invalid("context item canonical source"));
    }
    validate_source(rows, cut, source)?;
    let generation = cut.generation;
    super::context_state::apply(rows, generation, sequence, item, pinned, effective)?;
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
        decoded_bytes: super::encoding::decode_bytes(event)?,
    };
    totals.records = totals
        .records
        .checked_add(1)
        .ok_or(RecoveryError::Limit("active source count"))?;
    totals.serialized_bytes = totals
        .serialized_bytes
        .checked_add(source.serialized_bytes)
        .ok_or(RecoveryError::Limit("active source bytes"))?;
    totals.decoded_bytes = totals
        .decoded_bytes
        .checked_add(source.decoded_bytes)
        .ok_or(RecoveryError::Limit("active decoded bytes"))?;
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
    active.tool_lifecycle.decoded_bytes = active
        .tool_lifecycle
        .decoded_bytes
        .checked_add(super::encoding::decode_bytes(source)?)
        .ok_or(RecoveryError::Limit("tool lifecycle decoded bytes"))?;
    rows.put(key(ACTIVE_TOOL_LIFECYCLE, turn, sequence.0), source)
}

fn validate_source(
    rows: &BatchRows,
    cut: &super::ConversationCut,
    sequence: SequenceId,
) -> Result<(), RecoveryError> {
    let ordinal = rows
        .get::<u64>(key(SOURCE_ORDINAL, cut.generation, sequence.0))?
        .ok_or(RecoveryError::Invalid("context source is not effective"))?;
    let source = rows
        .get::<ConversationSource>(key(CONVERSATION, cut.generation, ordinal))?
        .ok_or(RecoveryError::Invalid("context source selector missing"))?;
    if ordinal >= cut.turns || source.sequence != sequence {
        return Err(RecoveryError::Invalid("context source is not effective"));
    }
    Ok(())
}
