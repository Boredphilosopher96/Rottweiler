//! Converts validated durable wire events into engine-owned recovery transitions.
#[allow(clippy::wildcard_imports)]
use super::*;

#[allow(clippy::match_same_arms, clippy::too_many_lines)]
pub(in crate::engine) fn recovered_pending_event(
    event: &EngineEvent,
) -> Result<Option<PendingEvent>, SessionProjectionError> {
    if event.delivery() != rw_types::EngineEventDelivery::Durable {
        return Err(SessionProjectionError::NonDurableEvent);
    }
    let pending = match event {
        EngineEvent::ProviderCallAccounted { call, actuals, .. } => {
            PendingEvent::ProviderCallAccounted {
                call: call.clone(),
                actuals: actuals.clone(),
            }
        }
        EngineEvent::TurnStarted { turn_id, .. } => PendingEvent::TurnStarted {
            turn: parse_turn_id(turn_id)?,
        },
        EngineEvent::MessageQueued {
            position,
            content,
            attachments,
            ..
        } => PendingEvent::MessageQueued {
            position: *position,
            content: content.clone(),
            attachments: attachments.clone(),
        },
        EngineEvent::QueuedMessageRemoved { position, .. } => PendingEvent::QueuedMessageRemoved {
            position: *position,
        },
        EngineEvent::QueuedMessagesCleared { .. } => PendingEvent::QueuedMessagesCleared,
        EngineEvent::UserMessageRetained {
            accepted_source, ..
        } => PendingEvent::UserMessageRetained {
            accepted_source: *accepted_source,
        },
        EngineEvent::UserMessageAccepted {
            agent_turn,
            content,
            attachments,
            ..
        } => PendingEvent::UserMessageAccepted {
            turn: *agent_turn,
            content: content.clone(),
            attachments: attachments.clone(),
        },
        EngineEvent::SessionTitleUpdated {
            title, usage, cost, ..
        } => PendingEvent::SessionTitleUpdated {
            title: title.clone(),
            usage: usage.as_ref().map(|usage| SessionUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            }),
            cost: cost.clone(),
        },
        EngineEvent::PluginMessageInjected {
            plugin_id,
            content,
            queued,
            ..
        } => PendingEvent::PluginMessageInjected {
            plugin_id: plugin_id.clone(),
            content: content.clone(),
            queued: *queued,
        },
        EngineEvent::PluginStatusChanged {
            plugin_id, status, ..
        } => PendingEvent::PluginStatusChanged {
            plugin_id: plugin_id.clone(),
            status: status.clone(),
        },
        EngineEvent::TodoStateCommitted { snapshot, .. } => PendingEvent::TodoStateCommitted {
            snapshot: snapshot.clone(),
        },
        EngineEvent::ExtensionStateCommitted {
            plugin_id,
            transaction,
            ..
        } => PendingEvent::ExtensionStateCommitted {
            plugin_id: plugin_id.clone(),
            transaction: transaction.clone(),
        },
        EngineEvent::UiNotification {
            plugin_id,
            title,
            message,
            ..
        } => PendingEvent::UiNotification {
            plugin_id: plugin_id.clone(),
            title: title.clone(),
            message: message.clone(),
        },
        EngineEvent::ConversationContextCommitted {
            agent_turn,
            selection,
            ..
        } => PendingEvent::ConversationContextCommitted {
            agent_turn: *agent_turn,
            selection: selection.clone(),
        },
        EngineEvent::ConversationInputCommitted {
            agent_turn,
            accepted_source,
            selection,
            ..
        } => PendingEvent::ConversationInputCommitted {
            agent_turn: *agent_turn,
            accepted_source: *accepted_source,
            selection: selection.clone(),
        },
        EngineEvent::ConversationTurnCommitted {
            agent_turn, turn, ..
        } => PendingEvent::ConversationTurnCommitted {
            agent_turn: *agent_turn,
            turn: turn.clone(),
        },
        EngineEvent::ConversationRewound {
            to_agent_turn,
            operation_id,
            unrestorable_paths,
            ..
        } => PendingEvent::ConversationRewound {
            to_turn: *to_agent_turn,
            operation_id: operation_id.clone(),
            unrestorable_paths: unrestorable_paths.clone(),
        },
        EngineEvent::TextDelta { turn_id, text, .. } => PendingEvent::TextDelta {
            turn: parse_turn_id(turn_id)?,
            text: text.clone(),
        },
        EngineEvent::ThinkingDelta {
            turn_id,
            text,
            signature,
            ..
        } => PendingEvent::ThinkingDelta {
            turn: parse_turn_id(turn_id)?,
            content: text.clone(),
            signature: signature.clone(),
        },
        EngineEvent::CitationDelta {
            turn_id,
            uri,
            title,
            ..
        } => PendingEvent::CitationDelta {
            turn: parse_turn_id(turn_id)?,
            uri: uri.clone(),
            title: title.clone(),
        },
        EngineEvent::ToolCallStarted {
            turn_id,
            tool_call_id,
            invocation_id,
            name,
            args,
            call_index,
            ..
        } => PendingEvent::ToolCallStarted {
            turn: parse_turn_id(turn_id)?,
            id: tool_call_id.0.clone(),
            invocation_id: invocation_id.clone(),
            name: name.clone(),
            arguments: args.clone(),
            index: usize::try_from(*call_index).unwrap_or(usize::MAX),
        },
        EngineEvent::ToolOutputDelta {
            turn_id,
            tool_call_id,
            invocation_id,
            stream,
            chunk,
            ..
        } => PendingEvent::ToolOutput {
            turn: parse_turn_id(turn_id)?,
            id: tool_call_id.0.clone(),
            invocation_id: invocation_id.clone(),
            stream: match stream {
                ToolOutputStream::Stdout => "stdout",
                ToolOutputStream::Stderr => "stderr",
            }
            .to_owned(),
            chunk: chunk.clone(),
        },
        EngineEvent::ToolDiffReady {
            turn_id,
            tool_call_id,
            invocation_id,
            diff,
            ..
        } => PendingEvent::ToolDiffReady {
            turn: parse_turn_id(turn_id)?,
            id: tool_call_id.0.clone(),
            invocation_id: invocation_id.clone(),
            diff: diff.clone(),
        },
        EngineEvent::ToolCallFinished {
            presentation,
            turn_id,
            tool_call_id,
            invocation_id,
            output,
            is_error,
            call_index,
            ..
        } => PendingEvent::ToolCallFinished {
            presentation: presentation.clone(),
            turn: parse_turn_id(turn_id)?,
            id: tool_call_id.0.clone(),
            invocation_id: invocation_id.clone(),
            output: output.clone(),
            is_error: *is_error,
            index: usize::try_from(*call_index).unwrap_or(usize::MAX),
        },
        EngineEvent::ToolApprovalResolved {
            turn_id,
            tool_call_id,
            invocation_id,
            decision,
            ..
        } => PendingEvent::ToolApprovalResolved {
            turn: parse_turn_id(turn_id)?,
            tool_call_id: tool_call_id.clone(),
            invocation_id: invocation_id.clone(),
            decision: decision.clone(),
        },
        EngineEvent::ToolApprovalNeeded {
            turn_id,
            tool_call_id,
            invocation_id,
            name,
            args,
            capabilities,
            diff,
            ..
        } => PendingEvent::PermissionRequested {
            turn: parse_turn_id(turn_id)?,
            request: PermissionRequest {
                id: tool_call_id.0.clone(),
                invocation_id: invocation_id.clone(),
                tool_name: name.clone(),
                arguments: args.clone(),
                capabilities: capabilities.clone(),
                approval_diff: diff.clone(),
            },
        },
        EngineEvent::QuestionAnswered {
            turn_id,
            question_id,
            answer,
            ..
        } => PendingEvent::QuestionAnswered {
            turn: parse_turn_id(turn_id)?,
            question_id: question_id.clone(),
            answer: answer.clone(),
        },
        EngineEvent::HookFailed {
            event,
            hook_id,
            fail_closed,
            message,
            ..
        } => PendingEvent::HookFailure {
            event: event.clone(),
            hook_id: hook_id.clone(),
            fail_closed: *fail_closed,
            message: message.clone(),
        },
        EngineEvent::CommandFinished {
            name,
            message,
            unrestorable_paths,
            ..
        } => PendingEvent::CommandFinished {
            name: name.clone(),
            message: message.clone(),
            unrestorable_paths: unrestorable_paths.clone(),
        },
        EngineEvent::GuardTriggered {
            turn_id,
            guard,
            message,
            ..
        } => PendingEvent::GuardTriggered {
            turn: parse_turn_id(turn_id)?,
            guard: guard.clone(),
            message: message.clone(),
        },
        EngineEvent::TurnFinished {
            turn_id,
            status,
            usage,
            cost,
            ..
        } => PendingEvent::TurnFinished {
            turn: parse_turn_id(turn_id)?,
            status: match status {
                TurnStatus::Completed => AgentTurnStatus::Completed,
                TurnStatus::Interrupted => AgentTurnStatus::Interrupted,
                TurnStatus::Failed => AgentTurnStatus::Failed,
                TurnStatus::MaxTurns => AgentTurnStatus::MaxTurns,
                TurnStatus::DoomLoop => AgentTurnStatus::DoomLoop,
                TurnStatus::BudgetExceeded => AgentTurnStatus::BudgetExceeded,
            },
            usage: SessionUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            },
            cost: cost.clone(),
        },
        EngineEvent::ContextUsageUpdated {
            turn_id,
            used_tokens,
            usable_tokens,
            reserved_tokens,
            context_window_known,
            context_window_reason,
            stable_prefix_hash,
            cache_hit_basis_points,
            estimated_input_tokens,
            provider_input_tokens,
            correction_millionths,
            ..
        } => PendingEvent::ContextUsage {
            turn: parse_turn_id(turn_id)?,
            used_tokens: *used_tokens,
            usable_tokens: *usable_tokens,
            reserved_tokens: *reserved_tokens,
            context_window_known: *context_window_known,
            context_window_reason: context_window_reason.clone(),
            stable_prefix_hash: stable_prefix_hash.clone(),
            cache_hit_basis_points: *cache_hit_basis_points,
            estimated_input_tokens: *estimated_input_tokens,
            provider_input_tokens: *provider_input_tokens,
            correction_millionths: *correction_millionths,
        },
        EngineEvent::BudgetStatusChanged {
            turn_id,
            level,
            scope,
            unit,
            current,
            limit,
            ..
        } => PendingEvent::BudgetStatus {
            turn: parse_turn_id(turn_id)?,
            level: level.clone(),
            scope: scope.clone(),
            unit: unit.clone(),
            current: *current,
            limit: *limit,
        },
        EngineEvent::CompactionStarted { reason, .. } => PendingEvent::CompactionStarted {
            reason: reason.clone(),
        },
        EngineEvent::CompactionAttemptFinished {
            summary_turn_id,
            usage,
            cost,
            ..
        } => PendingEvent::CompactionAttemptFinished {
            summary_turn: parse_turn_id(summary_turn_id)?,
            usage: SessionUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            },
            cost: cost.clone(),
        },
        EngineEvent::CompactionFinished {
            summary_turn_id,
            reclaimed_tokens,
            usage,
            cost,
            ..
        } => PendingEvent::CompactionFinished {
            summary_turn: parse_turn_id(summary_turn_id)?,
            reclaimed_tokens: *reclaimed_tokens,
            usage: usage.as_ref().map(|usage| SessionUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            }),
            cost: cost.clone(),
        },
        EngineEvent::CompactionFailed {
            summary_turn_id, ..
        } => PendingEvent::CompactionFailed {
            summary_turn: parse_turn_id(summary_turn_id)?,
        },
        EngineEvent::ToolOutputPruned {
            source,
            reclaimed_tokens,
            ..
        } => PendingEvent::ToolOutputPruned {
            source: *source,
            reclaimed_tokens: *reclaimed_tokens,
        },
        EngineEvent::ContextItemPinned {
            item_id,
            effective_after_agent_turn,
            ..
        } => PendingEvent::ContextItemPinned {
            item_id: item_id.clone(),
            effective_after_agent_turn: *effective_after_agent_turn,
        },
        EngineEvent::ContextItemEvicted {
            item_id,
            effective_after_agent_turn,
            ..
        } => PendingEvent::ContextItemEvicted {
            item_id: item_id.clone(),
            effective_after_agent_turn: *effective_after_agent_turn,
        },
        EngineEvent::DriverChanged {
            driver_client_id, ..
        } => PendingEvent::DriverChanged {
            driver_client_id: driver_client_id.clone(),
        },
        EngineEvent::SessionCreated {
            driver_client_id, ..
        } => PendingEvent::SessionCreated {
            driver_client_id: driver_client_id.clone(),
        },
        EngineEvent::WorkspaceRootsChanged {
            generation,
            effective_from_turn,
            roots,
            ..
        } => PendingEvent::WorkspaceRootsChanged {
            generation: *generation,
            effective_from_turn: *effective_from_turn,
            roots: roots.clone(),
        },
        EngineEvent::ModelChanged {
            model,
            provider,
            thinking,
            ..
        } => PendingEvent::ModelChanged {
            model: model.clone(),
            provider: provider.clone(),
            thinking: thinking.unwrap_or_default(),
        },
        EngineEvent::ModelContextCleared { strategy, .. } => PendingEvent::ModelContextCleared {
            strategy: *strategy,
        },
        EngineEvent::ModeChanged {
            mode,
            definition_fingerprint,
            ..
        } => PendingEvent::ModeChanged {
            mode: mode.clone(),
            definition_fingerprint: definition_fingerprint.clone(),
        },
        EngineEvent::PermissionModeChanged { mode, .. } => PendingEvent::PermissionModeChanged {
            mode: mode.as_deref().map(parse_permission_mode).transpose()?,
        },
        EngineEvent::PlanSubmitted { artifact, .. } => PendingEvent::PlanSubmitted {
            artifact: artifact.clone(),
        },
        EngineEvent::PlanReviewed {
            artifact,
            decision,
            revisions,
            ..
        } => PendingEvent::PlanReviewed {
            artifact: artifact.clone(),
            decision: *decision,
            revisions: revisions.clone(),
        },
        EngineEvent::UserShellStateChanged {
            shell_id,
            command,
            active,
            status,
            captured_output,
            ..
        } => PendingEvent::UserShellStateChanged {
            shell_id: shell_id.clone(),
            command: command.clone().unwrap_or_default(),
            active: *active,
            status: *status,
            captured_output: captured_output.clone(),
        },
        EngineEvent::QuestionAsked {
            turn_id,
            question_id,
            question,
            ..
        } => PendingEvent::QuestionAsked {
            turn: parse_turn_id(turn_id)?,
            question_id: question_id.clone(),
            question: question.clone(),
        },
        EngineEvent::Error { error, .. } => PendingEvent::Error {
            message: error.message.clone(),
        },
        EngineEvent::SubagentSpawned { .. } | EngineEvent::SubagentFinished { .. } => {
            return Ok(None);
        }
        _ => unreachable!("non-durable events were rejected before projection"),
    };
    Ok(Some(pending))
}
