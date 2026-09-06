use crate::engine::AgentLoopError;
use crate::engine::model::ModelContextMetadata;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::ContextSurgeryAction;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::provider_messages::persist_event;
use crate::engine::turn::provider_messages::tool_definition;
use crate::engine::turn::signals::TurnSignal;
use rw_context::AssembledContext;
use rw_context::AssemblyInput;
use rw_context::ContextAssembler;
use rw_context::ContextItem as AssemblyContextItem;
use rw_context::ContextItemId as AssemblyContextItemId;
use rw_context::ContextItemKind as AssemblyContextItemKind;
use rw_context::ContextProvenance;
use rw_context::LocalTokenEstimator;
use rw_context::OverflowPolicy;
use rw_context::PRUNED_TOOL_OUTPUT_REPLACEMENT;
use rw_context::PruneConfig;
use rw_context::PruneRecord;
use rw_context::PruneRecordKind;
use rw_context::Pruner;
use rw_context::ToonPromptEncoder;
use rw_providers::CacheBreakpointSupport;
use rw_types::Block;
use rw_types::CacheBreakpoint;
use rw_types::ContextItemId;
use rw_types::ContextItemKind;
use rw_types::ContextItemSnapshot;
use rw_types::ContextItemState;
use rw_types::ContextSnapshot;
use rw_types::ModelAlias;
use rw_types::PromptDump;
use rw_types::PromptTool;
use rw_types::Role;
use rw_types::ToolOutput;
use rw_types::ToolOutputPart;
use rw_types::Turn;
use rw_types::TurnId;
use rw_types::TurnMeta;
use rw_types::config::CompactionConfig;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use tokio::sync::mpsc;

pub(super) fn context_action_state(
    actions: &[ContextSurgeryAction],
    item_id: &ContextItemId,
) -> (bool, bool) {
    actions
        .iter()
        .rev()
        .find(|action| &action.item_id == item_id)
        .map_or((false, false), |action| {
            if action.pinned {
                (true, false)
            } else {
                (false, true)
            }
        })
}

pub(super) fn prompt_tool_output(
    output: &ToolOutput,
    is_pruned: bool,
    toon: &mut ToonPromptEncoder,
) -> ToolOutput {
    if is_pruned {
        return ToolOutput::Text {
            text: PRUNED_TOOL_OUTPUT_REPLACEMENT.to_owned(),
        };
    }
    match output {
        ToolOutput::Text { .. } => output.clone(),
        ToolOutput::Structured { value } => toon.encode(value).map_or_else(
            |_| output.clone(),
            |encoded| ToolOutput::Text {
                text: encoded.prompt_text,
            },
        ),
        ToolOutput::Mixed { parts } => ToolOutput::Mixed {
            parts: parts
                .iter()
                .map(|part| match part {
                    ToolOutputPart::Structured { value } => toon.encode(value).map_or_else(
                        |_| part.clone(),
                        |encoded| ToolOutputPart::Text {
                            text: encoded.prompt_text,
                        },
                    ),
                    ToolOutputPart::Text { .. } | ToolOutputPart::Image { .. } => part.clone(),
                })
                .collect(),
        },
    }
}

pub(in crate::engine) fn prompt_turn(
    turn: &Turn,
    pruned_tool_outputs: &BTreeMap<String, u64>,
    toon: &mut ToonPromptEncoder,
) -> Turn {
    let mut prompt = turn.clone();
    prompt.blocks = prompt
        .blocks
        .into_iter()
        .map(|block| match block {
            Block::ToolResult {
                id,
                output,
                is_error,
            } => {
                let is_pruned = pruned_tool_outputs.contains_key(&id.0);
                Block::ToolResult {
                    id,
                    output: prompt_tool_output(&output, is_pruned, toon),
                    is_error,
                }
            }
            other => other,
        })
        .collect();
    prompt
}

#[tracing::instrument(target = "rw_performance", level = "trace", name = "context.assemble", skip_all, fields(session_id = config.session_id.0.as_str(), turns = conversation.len()))]
pub(in crate::engine) fn assemble_session_context(
    config: &SessionActorConfig,
    conversation: &[Turn],
    queued: &VecDeque<String>,
    surgery: &[ContextSurgeryAction],
    pruned_tool_outputs: &BTreeMap<String, u64>,
) -> Result<AssembledContext, AgentLoopError> {
    let stable_prefix = config
        .initial_session_context
        .iter()
        .enumerate()
        .map(|(index, turn)| AssemblyContextItem {
            id: AssemblyContextItemId(format!("system:{index}")),
            kind: if index == 0 {
                AssemblyContextItemKind::System
            } else {
                AssemblyContextItemKind::ProjectInstructions
            },
            label: if index == 0 {
                "Base system instructions".to_owned()
            } else {
                format!("Project instructions {index}")
            },
            provenance: ContextProvenance::BuiltIn,
            turn: turn.clone(),
            pinned: false,
            evicted: false,
            summarized: false,
            pruned: false,
        })
        .collect();
    let mut toon = ToonPromptEncoder::default();
    let conversation = conversation
        .iter()
        .enumerate()
        .map(|(index, turn)| {
            let item_id = ContextItemId(format!("conversation:{index}"));
            let (pinned, evicted) = context_action_state(surgery, &item_id);
            let pruned = turn.blocks.iter().any(|block| {
                matches!(block, Block::ToolResult { id, .. } if pruned_tool_outputs.contains_key(&id.0))
            });
            AssemblyContextItem {
                id: AssemblyContextItemId(item_id.0),
                kind: if pinned {
                    AssemblyContextItemKind::Pin
                } else {
                    AssemblyContextItemKind::Conversation
                },
                label: format!("{:?} turn {}", turn.role, index.saturating_add(1)),
                provenance: if pinned {
                    ContextProvenance::UserPin
                } else {
                    ContextProvenance::Conversation {
                        sequence: u64::try_from(index).unwrap_or(u64::MAX),
                    }
                },
                turn: prompt_turn(turn, pruned_tool_outputs, &mut toon),
                pinned,
                evicted,
                summarized: turn.meta.summary,
                pruned,
            }
        })
        .collect();
    let queued = queued
        .iter()
        .enumerate()
        .map(|(index, content)| AssemblyContextItem {
            id: AssemblyContextItemId(format!("queued:{index}")),
            kind: AssemblyContextItemKind::Queued,
            label: format!("Queued message {}", index.saturating_add(1)),
            provenance: ContextProvenance::ClientQueue,
            turn: Turn {
                role: Role::User,
                blocks: vec![Block::Text {
                    text: content.clone(),
                }],
                meta: TurnMeta::default(),
            },
            pinned: false,
            evicted: false,
            summarized: false,
            pruned: false,
        })
        .collect();
    let metadata = config.model.context_metadata(&config.model_alias);
    ContextAssembler::assemble(AssemblyInput {
        stable_prefix,
        conversation,
        pins: Vec::new(),
        queued,
        tools: config
            .tools
            .descriptors()
            .into_iter()
            .map(tool_definition)
            .collect(),
        cache_support: metadata
            .cache_breakpoints
            .unwrap_or(CacheBreakpointSupport::None),
        include_prompt_dump: false,
    })
    .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))
}

pub(in crate::engine) fn protocol_context_kind(
    kind: AssemblyContextItemKind,
    role: Option<&Role>,
) -> ContextItemKind {
    match kind {
        AssemblyContextItemKind::System => ContextItemKind::System,
        AssemblyContextItemKind::ProjectInstructions => ContextItemKind::ProjectInstructions,
        AssemblyContextItemKind::SkillIndex => ContextItemKind::ToolDefinitions,
        AssemblyContextItemKind::Pin => ContextItemKind::Pinned,
        AssemblyContextItemKind::Queued => ContextItemKind::QueuedMessage,
        AssemblyContextItemKind::Conversation => {
            if role == Some(&Role::Tool) {
                ContextItemKind::ToolResult
            } else {
                ContextItemKind::Conversation
            }
        }
    }
}

pub(super) fn resolved_overflow_policy(
    metadata: ModelContextMetadata,
    compaction: &CompactionConfig,
) -> Result<Option<OverflowPolicy>, String> {
    let Some(context_window_tokens) = metadata.max_context_tokens else {
        return Ok(None);
    };
    OverflowPolicy {
        context_window_tokens,
        max_output_tokens: metadata.max_output_tokens.unwrap_or(0),
        reserved_tokens_override: compaction.reserved_tokens,
        automatic_compaction: compaction.auto,
    }
    .validate()
    .map(Some)
    .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_lines)]
pub(in crate::engine) fn context_snapshot(
    assembled: &AssembledContext,
    durable_conversation: &[Turn],
    pruned_tool_outputs: &BTreeMap<String, u64>,
    metadata: ModelContextMetadata,
    compaction: &CompactionConfig,
    turn_id: Option<TurnId>,
    through: Option<rw_types::SequenceId>,
) -> ContextSnapshot {
    let (policy, context_window_reason) = match resolved_overflow_policy(metadata, compaction) {
        Ok(Some(policy)) => (Some(policy), None),
        Ok(None) => (
            None,
            Some("provider did not report a context window".to_owned()),
        ),
        Err(error) => (None, Some(error)),
    };
    let context_window_known = policy.is_some();
    let (usable_tokens, reserved_tokens) = policy.map_or((0, 0), |policy| {
        let reserved = policy.reserved_tokens();
        (
            policy.context_window_tokens.saturating_sub(reserved),
            reserved,
        )
    });
    let mut items = assembled
        .items
        .iter()
        .filter(|item| {
            let Some(index) = item
                .id
                .0
                .strip_prefix("conversation:")
                .and_then(|index| index.parse::<usize>().ok())
            else {
                return true;
            };
            durable_conversation
                .get(index)
                .is_none_or(|turn| turn.role != Role::Tool)
        })
        .map(|item| {
            let (source, machine_local_path) = match &item.provenance {
                ContextProvenance::BuiltIn => ("built_in".to_owned(), None),
                ContextProvenance::ProjectFile { path } => {
                    ("project_file".to_owned(), Some(path.clone()))
                }
                ContextProvenance::Extension { extension_id } => {
                    (format!("extension:{extension_id}"), None)
                }
                ContextProvenance::Conversation { sequence } => {
                    (format!("conversation:{sequence}"), None)
                }
                ContextProvenance::UserPin => ("user_pin".to_owned(), None),
                ContextProvenance::ClientQueue => ("client_queue".to_owned(), None),
            };
            let role = item
                .assembled_turn_index
                .and_then(|index| assembled.turns.get(index))
                .map(|turn| &turn.role);
            ContextItemSnapshot {
                item_id: ContextItemId(item.id.0.clone()),
                kind: protocol_context_kind(item.kind, role),
                label: item.label.clone(),
                source,
                machine_local_path,
                estimated_tokens: item.tokens,
                state: ContextItemState {
                    pinned: item.pinned,
                    evicted: item.evicted,
                    summarized: item.summarized,
                    pruned: item.pruned,
                },
            }
        })
        .collect::<Vec<_>>();
    items.extend(assembled.tools.iter().map(|tool| ContextItemSnapshot {
        item_id: ContextItemId(format!("tool:{}", tool.name)),
        kind: ContextItemKind::ToolDefinitions,
        label: tool.name.clone(),
        source: "tool_registry".to_owned(),
        machine_local_path: None,
        estimated_tokens: LocalTokenEstimator::tools(std::slice::from_ref(tool)),
        state: ContextItemState {
            // Tool schemas are part of the provider request shape, but they
            // are not user pins and the context UI must not claim otherwise.
            pinned: false,
            evicted: false,
            summarized: false,
            pruned: false,
        },
    }));
    for (index, turn) in durable_conversation.iter().enumerate() {
        if turn.role != Role::Tool {
            continue;
        }
        let context_item_id = format!("conversation:{index}");
        let parent = assembled
            .items
            .iter()
            .find(|item| item.id.0 == context_item_id);
        let prompt_turn = parent
            .and_then(|item| item.assembled_turn_index)
            .and_then(|index| assembled.turns.get(index));
        for block in &turn.blocks {
            if let Block::ToolResult { id, .. } = block {
                let prompt_block = prompt_turn
                    .and_then(|turn| {
                        turn.blocks.iter().find(|block| {
                            matches!(block, Block::ToolResult { id: prompt_id, .. } if prompt_id == id)
                        })
                    })
                    .unwrap_or(block);
                items.push(ContextItemSnapshot {
                    item_id: ContextItemId(format!("tool_result:{}", id.0)),
                    kind: ContextItemKind::ToolResult,
                    label: format!("Tool result {}", id.0),
                    source: "conversation_tool_result".to_owned(),
                    machine_local_path: None,
                    estimated_tokens: LocalTokenEstimator::turn(&Turn {
                        role: Role::Tool,
                        blocks: vec![prompt_block.clone()],
                        meta: TurnMeta::default(),
                    }),
                    state: ContextItemState {
                        pinned: parent.is_some_and(|item| item.pinned),
                        evicted: parent.is_some_and(|item| item.evicted),
                        summarized: parent.is_some_and(|item| item.summarized),
                        pruned: pruned_tool_outputs.contains_key(&id.0),
                    },
                });
            }
        }
    }
    ContextSnapshot {
        through,
        turn_id,
        stable_prefix_hash: assembled.stable_prefix_hash.clone(),
        used_tokens: assembled.token_totals.total,
        usable_tokens,
        reserved_tokens,
        context_window_known,
        context_window_reason,
        cache_breakpoints: assembled
            .cache_breakpoints
            .iter()
            .map(|breakpoint| CacheBreakpoint {
                after_item_id: breakpoint
                    .after_item_id
                    .as_ref()
                    .map(|id| ContextItemId(id.0.clone())),
            })
            .collect(),
        items,
    }
}

pub(in crate::engine) fn prompt_dump(
    assembled: AssembledContext,
    model_alias: &str,
    turn_id: Option<TurnId>,
    through: Option<rw_types::SequenceId>,
) -> PromptDump {
    PromptDump {
        through,
        turn_id,
        model_alias: ModelAlias(model_alias.to_owned()),
        turns: assembled.turns,
        tools: assembled
            .tools
            .into_iter()
            .map(|tool| PromptTool {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect(),
        stable_prefix_hash: assembled.stable_prefix_hash,
        cache_breakpoints: assembled
            .cache_breakpoints
            .into_iter()
            .map(|breakpoint| CacheBreakpoint {
                after_item_id: breakpoint.after_item_id.map(|id| ContextItemId(id.0)),
            })
            .collect(),
        estimated_tokens: assembled.token_totals.total,
    }
}

pub(super) async fn prune_before_provider_request(
    conversation: &[Turn],
    context_surgery: &[ContextSurgeryAction],
    pruned_tool_outputs: &mut BTreeMap<String, u64>,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> Result<(), AgentLoopError> {
    let mut tool_names = BTreeMap::<String, String>::new();
    for conversation_turn in conversation {
        for block in &conversation_turn.blocks {
            if let Block::ToolCall { id, name, .. } = block {
                tool_names.insert(id.0.clone(), name.clone());
            }
        }
    }
    let mut records = Vec::new();
    let mut toon = ToonPromptEncoder::default();
    let prompt_conversation = conversation
        .iter()
        .map(|turn| prompt_turn(turn, pruned_tool_outputs, &mut toon))
        .collect::<Vec<_>>();
    for (turn_index, (conversation_turn, prompt_conversation_turn)) in
        conversation.iter().zip(&prompt_conversation).enumerate()
    {
        let context_id = ContextItemId(format!("conversation:{turn_index}"));
        let (pinned, evicted) = context_action_state(context_surgery, &context_id);
        if evicted {
            records.push(PruneRecord {
                item_id: context_id.0,
                transcript_index: records.len(),
                kind: PruneRecordKind::PrunedMarker,
                tokens: 0,
                pinned: false,
            });
            continue;
        }
        if conversation_turn.meta.summary {
            records.push(PruneRecord {
                item_id: context_id.0.clone(),
                transcript_index: records.len(),
                kind: PruneRecordKind::SummaryMarker,
                tokens: LocalTokenEstimator::turn(prompt_conversation_turn),
                pinned,
            });
            continue;
        }
        if conversation_turn.role == Role::User {
            records.push(PruneRecord {
                item_id: context_id.0.clone(),
                transcript_index: records.len(),
                kind: PruneRecordKind::User,
                tokens: LocalTokenEstimator::turn(prompt_conversation_turn),
                pinned,
            });
        }
        for (block, prompt_block) in conversation_turn
            .blocks
            .iter()
            .zip(&prompt_conversation_turn.blocks)
        {
            let Block::ToolResult { id, .. } = block else {
                continue;
            };
            let tokens = LocalTokenEstimator::turn(&Turn {
                role: Role::Tool,
                blocks: vec![prompt_block.clone()],
                meta: TurnMeta::default(),
            });
            let already_pruned = pruned_tool_outputs.contains_key(&id.0);
            records.push(PruneRecord {
                item_id: format!("{}:tool:{}", context_id.0, id.0),
                transcript_index: records.len(),
                kind: if already_pruned {
                    PruneRecordKind::PrunedMarker
                } else {
                    PruneRecordKind::ToolOutput {
                        tool_call_id: id.0.clone(),
                        tool_name: tool_names
                            .get(&id.0)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_owned()),
                        completed: true,
                    }
                },
                tokens,
                pinned,
            });
        }
    }
    let plan = Pruner::plan(&records, &PruneConfig::default());
    for decision in plan.decisions {
        persist_event(
            signals,
            PendingEvent::ToolOutputPruned {
                tool_call_id: decision.tool_call_id.clone(),
                reclaimed_tokens: decision.original_tokens,
            },
        )
        .await?;
        pruned_tool_outputs.insert(decision.tool_call_id, decision.original_tokens);
    }
    Ok(())
}
