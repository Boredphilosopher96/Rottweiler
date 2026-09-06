#![cfg(test)]
use crate::engine::{
    builtin_hook_dispatcher,
    projection::ContextSurgeryAction,
    recovery::{ConversationSource, HistoryRead, TurnSourceKind},
    session::SessionActorConfig,
    tests::fixtures::{
        models::ScriptedModel,
        support::{config, text_turn},
        tools::{StubOutcome, StubTool},
    },
    turn::{
        context::{assemble_full_session_context, assemble_session_context},
        context_memory::{admit, readmit},
    },
};
use rw_providers::{ProviderRequest, ToolChoice};
use rw_tools::ToolRegistry;
use rw_types::{
    Block, ContextBlockId, Role, SequenceId, ToolCallId, ToolOutput, Turn, TurnMeta,
    config::PermissionDecision,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

fn source(sequence: u64) -> ConversationSource {
    ConversationSource {
        sequence: SequenceId(sequence),
        has_resolved_model: false,
        kind: TurnSourceKind::Committed,
        agent_turn: 1,
        role: Role::Tool,
        serialized_bytes: 0,
        decoded_bytes: 0,
        estimated_tokens: 0,
        cumulative_bytes: 0,
        cumulative_decoded_bytes: 0,
        cumulative_tokens: 0,
    }
}
fn output(index: usize) -> Turn {
    Turn {
        role: Role::Tool,
        blocks: vec![Block::ToolResult {
            id: ToolCallId("reused-provider-alias".into()),
            is_error: false,
            output: ToolOutput::Structured {
                value: serde_json::json!({"rows": (0..16).map(|row| serde_json::json!({"row":row,"value":format!("item {index}: αβ\\\"\n") })).collect::<Vec<_>>()}),
            },
        }],
        meta: TurnMeta::default(),
    }
}
fn fixture(root: &std::path::Path, generation: usize) -> SessionActorConfig {
    let mut tools = ToolRegistry::new();
    if generation >= 2 {
        tools
            .register(Arc::new(StubTool::new(
                "inspect",
                Vec::new(),
                StubOutcome::Success(rw_tools::ToolResult::new("ok", serde_json::Value::Null)),
            )))
            .expect("tool");
    }
    let mut result = config(
        root,
        Arc::new(ScriptedModel::default()),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    )
    .inner;
    result.initial_session_context = vec![text_turn(
        Role::System,
        format!("Instructions generation {generation}"),
    )];
    if generation >= 1 {
        result.model_alias = "other-model".into();
    }
    result
}
fn request_bytes(config: &SessionActorConfig, assembled: rw_context::AssembledContext) -> Vec<u8> {
    serde_json::to_vec(&ProviderRequest {
        model: config.model_alias.clone(),
        turns: assembled.turns,
        tools: assembled.tools,
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: config.max_output_tokens,
        temperature: None,
        thinking: config.thinking,
        cache_hint: None,
    })
    .expect("request bytes")
}

#[test]
fn incremental_context_matches_full_requests_across_source_and_configuration_changes() {
    let root = tempfile::tempdir().expect("root");
    // Keep every configuration alive so identity is exact and cannot be recycled.
    let configs = (0..3)
        .map(|generation| fixture(root.path(), generation))
        .collect::<Vec<_>>();
    let mut conversation = (0..12).map(output).collect::<Vec<_>>();
    let mut sources = (1..=12).map(source).collect::<Vec<_>>();
    let queued = VecDeque::from(["queued input".into()]);
    let mut working = admit(
        HistoryRead::new((), ()),
        &configs[0],
        &conversation,
        &queued,
    )
    .expect("admitted");
    let mut pruned = BTreeMap::new();
    let mut surgery = Vec::new();
    for step in 0..9 {
        let config = &configs[(step / 3).min(2)];
        match step {
            1 => {
                pruned.insert(
                    ContextBlockId {
                        sequence: SequenceId(1),
                        block_index: 0,
                    }
                    .key(),
                    10,
                );
            }
            2 => surgery.push(ContextSurgeryAction {
                item_id: rw_types::context_source::conversation_item(SequenceId(2)),
                pinned: true,
                effective_after_agent_turn: 1,
            }),
            4 => surgery.push(ContextSurgeryAction {
                item_id: rw_types::context_source::conversation_item(SequenceId(3)),
                pinned: false,
                effective_after_agent_turn: 1,
            }),
            5 => {
                conversation.push(output(12));
                sources.push(source(13));
            }
            7 => {
                conversation = vec![text_turn(Role::Assistant, "compacted summary"), output(99)];
                sources = vec![source(40), source(41)];
                pruned.clear();
                surgery.clear();
            }
            8 => {
                conversation.remove(1);
                sources.remove(1);
                conversation.push(output(100));
                sources.push(source(50));
            }
            _ => {}
        }
        working = readmit(working, config, &conversation, &queued).expect("replanned");
        for repetition in 0..2 {
            let prior_normalizations = working.normalizations();
            let cached = assemble_session_context(
                config,
                &working,
                &conversation,
                &sources,
                &queued,
                &surgery,
                &pruned,
            )
            .expect("cached");
            if repetition == 1 {
                assert_eq!(
                    working.normalizations(),
                    prior_normalizations,
                    "unchanged source normalization must be reused"
                );
            }
            let full = assemble_full_session_context(
                config,
                &conversation,
                &sources,
                &queued,
                &surgery,
                &pruned,
            )
            .expect("full");
            assert_eq!(cached, full, "metadata and tokens step {step}");
            assert_eq!(
                request_bytes(config, cached),
                request_bytes(config, full),
                "provider bytes step {step}"
            );
        }
    }
}

/// Run only on a quiet host; emits raw paired samples for external p99 analysis.
#[test]
#[ignore = "paired context CPU measurements require a quiet host"]
fn measure_incremental_context_against_full_assembly() {
    let root = tempfile::tempdir().expect("root");
    let config = fixture(root.path(), 2);
    let conversation = (0..128).map(output).collect::<Vec<_>>();
    let sources = (1..=128).map(source).collect::<Vec<_>>();
    let queued = VecDeque::new();
    let pruned = BTreeMap::new();
    let working =
        admit(HistoryRead::new((), ()), &config, &conversation, &queued).expect("working");
    drop(
        assemble_session_context(
            &config,
            &working,
            &conversation,
            &sources,
            &queued,
            &[],
            &pruned,
        )
        .expect("warm"),
    );
    for sample in 0..200 {
        for cached in if sample % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        } {
            let started = std::time::Instant::now();
            let result = if cached {
                assemble_session_context(
                    &config,
                    &working,
                    &conversation,
                    &sources,
                    &queued,
                    &[],
                    &pruned,
                )
            } else {
                assemble_full_session_context(
                    &config,
                    &conversation,
                    &sources,
                    &queued,
                    &[],
                    &pruned,
                )
            }
            .expect("assembly");
            std::hint::black_box(&result);
            eprintln!(
                "context_sample,{sample},{cached},{}",
                started.elapsed().as_nanos()
            );
        }
    }
}

#[test]
fn prefix_replacement_keeps_an_older_source_cache_entry() {
    let root = tempfile::tempdir().expect("root");
    let config = fixture(root.path(), 0);
    let mut conversation = vec![text_turn(Role::Assistant, "prefix"), output(1)];
    let mut sources = vec![source(1), source(2)];
    let queued = VecDeque::new();
    let pruned = BTreeMap::new();
    let mut working =
        admit(HistoryRead::new((), ()), &config, &conversation, &queued).expect("working");
    drop(
        assemble_session_context(
            &config,
            &working,
            &conversation,
            &sources,
            &queued,
            &[],
            &pruned,
        )
        .expect("warm"),
    );
    let before = working.normalizations();
    conversation[0] = text_turn(Role::Assistant, "new summary");
    sources[0] = source(40);
    working = readmit(working, &config, &conversation, &queued).expect("replanned");
    let cached = assemble_session_context(
        &config,
        &working,
        &conversation,
        &sources,
        &queued,
        &[],
        &pruned,
    )
    .expect("replacement");
    assert_eq!(
        working.normalizations(),
        before + 1,
        "unchanged older suffix is reused"
    );
    let full =
        assemble_full_session_context(&config, &conversation, &sources, &queued, &[], &pruned)
            .expect("oracle");
    assert_eq!(cached, full);
}
