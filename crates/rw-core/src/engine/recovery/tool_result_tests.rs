#![allow(clippy::expect_used)]
use super::{
    tests::{append, catch_up, finish},
    *,
};
use crate::engine::{PendingEvent, project_session_events};
use rw_ext::ModeRegistry;
use rw_store::session::journal::{JournalAppendPlan, SegmentedJournal};
use rw_types::{
    Block, EngineEvent, EventMeta, Role, SequenceId, ToolCallId, ToolOutput, Turn, TurnMeta,
    tool_result_admission::ToolResultAdmission,
};

fn output(text: &str) -> Turn {
    Turn {
        role: Role::Tool,
        blocks: vec![Block::ToolResult {
            id: ToolCallId("provider-alias".into()),
            output: ToolOutput::Text { text: text.into() },
            is_error: false,
        }],
        meta: TurnMeta::default(),
    }
}
fn meta(sequence: u64) -> EventMeta {
    EventMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        session_id: rw_types::SessionId("canonical".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-06T00:00:00.000Z".into(),
        caused_by: None,
    }
}
fn seed(turn: &Turn) -> Vec<PendingEvent> {
    let mut events = vec![PendingEvent::TurnStarted { turn: 1 }];
    events.extend(crate::engine::tool_result_fixture::events(1, 1, turn));
    events
}
fn wire(events: Vec<PendingEvent>) -> Vec<EngineEvent> {
    events
        .into_iter()
        .enumerate()
        .map(|(sequence, event)| event.stamp(meta(sequence as u64)))
        .collect()
}

#[test]
fn result_body_has_one_durable_owner_and_exact_provider_ir_after_reopen() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let turn = output("one-authoritative-body-\n\\\"-é");
    let mut events = seed(&turn);
    events.push(finish(1));
    append(&mut journal, events);
    let source = journal.read_view();
    let events = source
        .page::<EngineEvent>(None, rw_store::session::SessionEventPageLimits::default())
        .expect("source")
        .events;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, EngineEvent::ToolCallFinished { .. }))
            .count(),
        1
    );
    assert!(events.iter().all(|event| !matches!(&event.event, EngineEvent::ConversationTurnCommitted { turn, .. } if turn.role == Role::Tool)));
    let bytes = serde_json::to_string(&events).expect("encoded source");
    assert_eq!(bytes.matches("one-authoritative-body").count(), 1);
    let audit = project_session_events(
        &events
            .iter()
            .map(|event| event.event.clone())
            .collect::<Vec<_>>(),
    )
    .expect("audit");
    assert_eq!(audit.conversation, vec![turn.clone()]);
    let modes = ModeRegistry::builtins().expect("modes");
    let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("recovery");
    catch_up(&mut recovery, &source, &modes);
    drop(recovery);
    let reopened = CanonicalRecovery::open(&source, &modes, None).expect("reopen");
    let history = reopened
        .snapshot()
        .expect("snapshot")
        .bind_source(&source)
        .expect("binding");
    assert_eq!(
        history
            .materialize(0..1, HistoryMaterializationLimits::default())
            .expect("provider IR"),
        vec![turn]
    );
    assert_eq!(
        history.turn_source(0).expect("source").sequence,
        SequenceId(3)
    );
}

#[test]
fn unclaimed_reordered_foreign_and_forged_tool_result_references_are_rejected() {
    let mut turn = output("first");
    turn.blocks.push(Block::ToolResult {
        id: ToolCallId("second".into()),
        output: ToolOutput::Text {
            text: "second".into(),
        },
        is_error: true,
    });
    let valid = wire(seed(&turn));
    assert!(project_session_events(&valid).is_ok());
    for mutation in 0..7 {
        let mut events = valid.clone();
        let EngineEvent::ConversationToolResultsCommitted {
            agent_turn,
            results,
            logical,
            ..
        } = events.last_mut().expect("commit")
        else {
            panic!("commit");
        };
        match mutation {
            0 => results.reverse(),
            1 => results[1] = results[0].clone(),
            2 => results[0].finished_source = SequenceId(1),
            3 => results[0].invocation_id.0 = "foreign".into(),
            4 => *agent_turn = 2,
            5 => logical.encoded_bytes += 1,
            6 => {
                results.pop();
            }
            _ => unreachable!(),
        }
        assert!(
            project_session_events(&events).is_err(),
            "mutation {mutation}"
        );
    }
    let mut reused = valid.clone();
    let mut commit = valid.last().expect("commit").clone();
    commit.meta_mut().expect("metadata").sequence_id = SequenceId(reused.len() as u64);
    reused.push(commit);
    assert!(
        project_session_events(&reused).is_err(),
        "a completed source is one-use"
    );
    let embedded = wire(vec![
        PendingEvent::TurnStarted { turn: 1 },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn,
        },
    ]);
    assert!(
        project_session_events(&embedded).is_err(),
        "embedded Tool bodies have no authority"
    );
}

#[test]
fn logical_receipt_matches_the_existing_journal_envelope_admission() {
    for text in ["plain", "\\\"\né", "\u{0000}"] {
        let turn = output(text);
        let logical = ToolResultAdmission::measure(&turn).expect("logical profile");
        let event = EngineEvent::ConversationTurnCommitted {
            meta: meta(7),
            agent_turn: 1,
            turn,
        };
        let events = [event];
        let plan = JournalAppendPlan::measure(SequenceId(7), &events).expect("canonical envelope");
        plan.encode(&events).expect("canonical decoder admission");
        super::tool_results::validate_admission(&meta(7), 1, &logical).expect("same admission");
        let mut forged = logical;
        forged.nodes = u32::MAX;
        assert!(super::tool_results::validate_admission(&meta(7), 1, &forged).is_err());
    }
}

#[test]
fn individually_legal_completions_cannot_amplify_the_logical_ir_limit() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(&mut journal, vec![PendingEvent::TurnStarted { turn: 1 }]);
    let mut refs = Vec::new();
    for index in 0..2 {
        let mut events = crate::engine::tool_result_fixture::events(
            1 + index * 2,
            1,
            &output(&"x".repeat(8 * 1024 * 1024)),
        );
        let PendingEvent::ConversationToolResultsCommitted { results, .. } =
            events.pop().expect("selector")
        else {
            panic!("selector");
        };
        refs.extend(results);
        append(&mut journal, events);
    }
    append(
        &mut journal,
        vec![PendingEvent::ConversationToolResultsCommitted {
            agent_turn: 1,
            results: refs,
            logical: ToolResultAdmission::measure(&output("forged-small-profile"))
                .expect("profile"),
        }],
    );
    let modes = ModeRegistry::builtins().expect("modes");
    let source = journal.read_view();
    let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("index");
    for _ in 0..8 {
        match recovery.advance(&source, &modes) {
            Err(_) => return,
            Ok(progress) => assert!(progress.has_more, "oversized source was published"),
        }
    }
    panic!("bounded fixture did not reach its result selector");
}

#[test]
fn missing_finished_checkpoint_field_is_rejected() {
    let mut state = serde_json::to_value(rw_types::input_claims::InputClaimState::default())
        .expect("checkpoint");
    state.as_object_mut().expect("object").remove("finished");
    assert!(serde_json::from_value::<rw_types::input_claims::InputClaimState>(state).is_err());
}
