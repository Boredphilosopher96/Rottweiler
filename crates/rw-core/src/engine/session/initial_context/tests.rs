#![allow(clippy::expect_used)]
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Allowance {
    live: Arc<AtomicUsize>,
    bytes: usize,
    limit: usize,
}
impl HistoryWorkingAllowance for Allowance {
    fn resize(&mut self, bytes: usize) -> Result<(), AgentLoopError> {
        if bytes > self.limit {
            return Err(invalid());
        }
        self.live.fetch_sub(self.bytes, Ordering::SeqCst);
        self.live.fetch_add(bytes, Ordering::SeqCst);
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for Allowance {
    fn drop(&mut self) {
        self.live.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}
fn allowance(live: &Arc<AtomicUsize>, limit: usize) -> Box<dyn HistoryWorkingAllowance> {
    Box::new(Allowance {
        live: Arc::clone(live),
        bytes: 0,
        limit,
    })
}
fn turn(role: Role, text: &str) -> Turn {
    Turn {
        role,
        blocks: vec![Block::Text { text: text.into() }],
        meta: TurnMeta::default(),
    }
}
fn source(turns: Vec<Turn>, live: &Arc<AtomicUsize>) -> InitialSessionContext {
    let mut source = allowance(live, usize::MAX);
    source
        .resize(turns.prepared_bytes().expect("source size"))
        .expect("admission");
    InitialSessionContext::from_owned(HistoryRead::new(turns, source), allowance(live, usize::MAX))
        .expect("context")
}
fn texts(context: &InitialSessionContext) -> Vec<Vec<&str>> {
    context
        .iter()
        .map(|turn| {
            turn.blocks
                .iter()
                .map(|block| {
                    let Block::Text { text } = block else {
                        panic!("text")
                    };
                    text.as_str()
                })
                .collect()
        })
        .collect()
}

#[test]
fn clone_retains_exact_source_and_credit_without_body_copy() {
    let live = Arc::new(AtomicUsize::new(0));
    let context = source(vec![turn(Role::System, "policy")], &live);
    let pointer = std::ptr::from_ref(context.iter().next().expect("source"));
    let charged = live.load(Ordering::SeqCst);
    let cloned = context.clone();
    assert_eq!(charged, live.load(Ordering::SeqCst));
    drop(context);
    assert_eq!(
        pointer,
        std::ptr::from_ref(cloned.iter().next().expect("shared source"))
    );
    assert_eq!(charged, live.load(Ordering::SeqCst));
    drop(cloned);
    assert_eq!(0, live.load(Ordering::SeqCst));
}

#[test]
fn policy_overlay_copies_only_first_system_turn_and_preserves_order() {
    let live = Arc::new(AtomicUsize::new(0));
    let context = source(
        vec![
            turn(Role::User, "before"),
            turn(Role::System, "policy"),
            turn(Role::System, "other"),
        ],
        &live,
    );
    let pointers = context.iter().map(std::ptr::from_ref).collect::<Vec<_>>();
    let mut changed = context.clone();
    changed
        .append_system_text("mode", allowance(&live, usize::MAX))
        .expect("overlay");
    let changed_pointers = changed.iter().map(std::ptr::from_ref).collect::<Vec<_>>();
    assert_eq!(pointers[0], changed_pointers[0]);
    assert_ne!(pointers[1], changed_pointers[1]);
    assert_eq!(pointers[2], changed_pointers[2]);
    assert_eq!(
        texts(&changed),
        vec![vec!["before"], vec!["policy", "mode"], vec!["other"]]
    );
    assert_eq!(
        texts(&context),
        vec![vec!["before"], vec!["policy"], vec!["other"]]
    );
    drop(context);
    assert!(live.load(Ordering::SeqCst) > 0);
    drop(changed);
    assert_eq!(0, live.load(Ordering::SeqCst));
}

#[test]
fn failed_overlay_preserves_source_and_charge() {
    let live = Arc::new(AtomicUsize::new(0));
    let mut context = source(vec![turn(Role::System, "policy")], &live);
    let charged = live.load(Ordering::SeqCst);
    let pointer = std::ptr::from_ref(context.iter().next().expect("source"));
    assert!(
        context
            .append_system_text("mode", allowance(&live, 0))
            .is_err()
    );
    assert_eq!(charged, live.load(Ordering::SeqCst));
    assert_eq!(
        pointer,
        std::ptr::from_ref(context.iter().next().expect("source"))
    );
    assert_eq!(texts(&context), vec![vec!["policy"]]);
}

#[test]
fn appended_provider_body_and_source_metadata_survive_replaced_table() {
    let live = Arc::new(AtomicUsize::new(0));
    let mut original = InitialSessionContext::default();
    let mut credit = allowance(&live, usize::MAX);
    credit.resize(4_096).expect("producer admission");
    original
        .append_owned(
            HistoryRead::new(turn(Role::System, "provider"), credit),
            allowance(&live, usize::MAX),
        )
        .expect("append");
    let pointer = std::ptr::from_ref(original.iter().next().expect("source"));
    let mut other = source(vec![turn(Role::System, "ordinary")], &live);
    other
        .append_context(&original, allowance(&live, usize::MAX))
        .expect("share");
    drop(original);
    assert_eq!(
        pointer,
        std::ptr::from_ref(other.iter().nth(1).expect("provider"))
    );
    other
        .append_system_text("mode", allowance(&live, usize::MAX))
        .expect("overlay");
    assert_eq!(
        pointer,
        std::ptr::from_ref(other.iter().nth(1).expect("provider"))
    );
    assert!(live.load(Ordering::SeqCst) >= 4_096);
    drop(other);
    assert_eq!(0, live.load(Ordering::SeqCst));
}

#[test]
fn missing_system_policy_is_inserted_before_other_sources() {
    let live = Arc::new(AtomicUsize::new(0));
    let mut context = source(vec![turn(Role::User, "input")], &live);
    context
        .append_system_text("policy", allowance(&live, usize::MAX))
        .expect("policy");
    assert_eq!(texts(&context), vec![vec!["policy"], vec!["input"]]);
}

fn reserved_map() -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(262_144);
    map.insert("removed".into(), serde_json::Value::Null);
    map.remove("removed");
    map.insert("z".into(), serde_json::json!([{"inner": 1}]));
    map.insert("a".into(), serde_json::Value::Bool(true));
    serde_json::Value::Object(map)
}
fn opaque_source(live: &Arc<AtomicUsize>, mixed: bool) -> InitialSessionContext {
    use rw_types::{ToolCallId, ToolOutput, ToolOutputPart};
    let block = if mixed {
        Block::ToolResult {
            id: ToolCallId("tool".into()),
            output: ToolOutput::Mixed {
                parts: vec![ToolOutputPart::Structured {
                    value: reserved_map(),
                }],
            },
            is_error: false,
        }
    } else {
        Block::ToolCall {
            id: ToolCallId("tool".into()),
            name: "tool".into(),
            args: reserved_map(),
        }
    };
    let value = Turn {
        role: Role::System,
        blocks: vec![block],
        meta: TurnMeta::default(),
    };
    // The source producer owns its deliberately oversized opaque map. Overlay
    // admission is separate and may only cover normalized destination storage.
    let mut retained = allowance(live, usize::MAX);
    retained
        .resize(64 * 1024 * 1024)
        .expect("walk fixture invariant");
    InitialSessionContext::from_owned(
        HistoryRead::new(vec![value], retained),
        allowance(live, usize::MAX),
    )
    .expect("walk fixture invariant")
}

#[test]
fn policy_overlay_rebuilds_opaque_maps_and_preserves_wire_order() {
    let live = Arc::new(AtomicUsize::new(0));
    for mixed in [false, true] {
        let mut context = opaque_source(&live, mixed);
        let original = context.clone();
        let expected =
            serde_json::to_string(original.iter().next().expect("walk fixture invariant"))
                .expect("walk fixture invariant");
        let before = live.load(Ordering::SeqCst);
        context
            .append_system_text("policy", allowance(&live, 64 * 1024))
            .expect("walk fixture invariant");
        let changed = context.iter().next().expect("walk fixture invariant");
        assert_eq!(changed.blocks.len(), 2);
        assert_eq!(
            serde_json::to_string(&changed.blocks[0]).expect("walk fixture invariant"),
            serde_json::to_string(
                &original
                    .iter()
                    .next()
                    .expect("walk fixture invariant")
                    .blocks[0]
            )
            .expect("walk fixture invariant")
        );
        assert_eq!(
            serde_json::to_string(original.iter().next().expect("walk fixture invariant"))
                .expect("walk fixture invariant"),
            expected
        );
        assert!(
            changed.prepared_bytes().expect("walk fixture invariant")
                <= live.load(Ordering::SeqCst) - before
        );
        drop(context);
        drop(original);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }
}

#[cfg(feature = "allocation-measurement")]
#[test]
#[ignore = "requires isolated allocation counters: --exact --ignored --test-threads=1"]
fn opaque_map_overlay_physical_allocations_fit_admission() {
    let live = Arc::new(AtomicUsize::new(0));
    for mixed in [false, true] {
        let mut context = opaque_source(&live, mixed);
        let original = context.clone();
        let before = live.load(Ordering::SeqCst);
        let region = stats_alloc::Region::new(&stats_alloc::INSTRUMENTED_SYSTEM);
        context
            .append_system_text("policy", allowance(&live, 64 * 1024))
            .expect("walk fixture invariant");
        let change = region.change();
        let admitted = live.load(Ordering::SeqCst) - before;
        assert!(
            change.bytes_allocated <= admitted,
            "opaque source capacity escaped copy admission: allocated={} admitted={admitted}",
            change.bytes_allocated
        );
        drop(original);
        drop(context);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }
}
