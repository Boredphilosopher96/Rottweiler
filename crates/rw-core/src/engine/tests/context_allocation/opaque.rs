//! Exercise stable-prefix assembly with source-owned opaque map capacity.
use super::{MeasuredAllowance, Owner, admit, config, history};
use crate::engine::{
    builtin_hook_dispatcher, tests::fixtures::models::ScriptedModel,
    turn::context::assemble_session_context,
};
use rw_tools::ToolRegistry;
use rw_types::{Block, Role, ToolCallId, ToolOutput, ToolOutputPart, Turn, TurnMeta};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, atomic::AtomicBool},
};

fn reserved_map() -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(262_144);
    map.insert("removed".into(), serde_json::Value::Null);
    map.remove("removed");
    map.insert("z".into(), serde_json::json!([{"inner": 1}]));
    map.insert("a".into(), serde_json::Value::Bool(true));
    serde_json::Value::Object(map)
}

fn assemble_reserved_source() -> (usize, usize) {
    let root = tempfile::tempdir().expect("root");
    let mut config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        rw_types::config::PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    )
    .inner;
    let source = Turn {
        role: Role::System,
        blocks: vec![
            Block::ToolCall {
                id: ToolCallId("tool".into()),
                name: "tool".into(),
                args: reserved_map(),
            },
            Block::ToolResult {
                id: ToolCallId("tool".into()),
                output: ToolOutput::Mixed {
                    parts: vec![ToolOutputPart::Structured {
                        value: reserved_map(),
                    }],
                },
                is_error: false,
            },
        ],
        meta: TurnMeta::default(),
    };
    let expected = serde_json::to_string(&source).expect("source wire");
    config.initial_session_context = history::initial_context(vec![source]);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let working = admit(
        Box::new(MeasuredAllowance {
            requests: requests.clone(),
            limit: 128 * 1024 * 1024,
            _owner: Owner(Arc::new(AtomicBool::new(false))),
        }),
        &config,
        &[],
        &[],
        &VecDeque::new(),
    )
    .expect("assembly admission");
    let admitted = *requests
        .lock()
        .expect("requests")
        .last()
        .expect("allowance");
    #[cfg(feature = "allocation-measurement")]
    let region = stats_alloc::Region::new(&stats_alloc::INSTRUMENTED_SYSTEM);
    let assembled = assemble_session_context(
        &config,
        &working,
        &[],
        &[],
        &VecDeque::new(),
        &[],
        &BTreeMap::new(),
    )
    .expect("assembly");
    #[cfg(feature = "allocation-measurement")]
    let allocated = region.change().bytes_allocated;
    #[cfg(not(feature = "allocation-measurement"))]
    let allocated = 0;
    assert_eq!(assembled.turns.len(), 1);
    assert_eq!(
        serde_json::to_string(&assembled.turns[0]).expect("assembled wire"),
        expected
    );
    assert_eq!(
        serde_json::to_string(
            config
                .initial_session_context
                .iter()
                .next()
                .expect("retained source")
        )
        .expect("source wire"),
        expected
    );
    (allocated, admitted)
}

#[test]
fn stable_assembly_preserves_reserved_map_source_and_wire_order() {
    let _ = assemble_reserved_source();
}

#[cfg(feature = "allocation-measurement")]
#[test]
#[ignore = "requires isolated allocation counters: --exact --ignored --test-threads=1"]
fn stable_assembly_physical_allocations_fit_admission() {
    let (allocated, admitted) = assemble_reserved_source();
    assert!(
        allocated <= admitted,
        "stable assembly allocated {allocated} bytes under {admitted}-byte admission"
    );
}
