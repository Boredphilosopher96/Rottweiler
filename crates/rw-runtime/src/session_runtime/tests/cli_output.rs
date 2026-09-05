#![cfg(test)]
use super::ApprovalDecision;
use super::EngineEvent;
use super::EventMeta;
use super::PendingInteraction;
use super::QuestionId;
use super::SESSION_EVENT_VERSION;
use super::SequenceId;
use super::SessionId;
use super::ToolCapability;
use super::TurnId;
use super::VecDeque;
use super::parse_approval;
use super::public_cli_event;

#[test]
fn headless_approval_parser_fails_closed() {
    assert_eq!(parse_approval("yes"), ApprovalDecision::AllowOnce);
    assert_eq!(parse_approval("session"), ApprovalDecision::AllowSession);
    assert_eq!(parse_approval("project"), ApprovalDecision::AllowProject);
    assert_eq!(parse_approval("anything else"), ApprovalDecision::Deny);
}

#[test]
fn public_cli_json_drops_opaque_reasoning_signatures() {
    let event = EngineEvent::ThinkingDelta {
        meta: EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: SessionId("reasoning-output".to_owned()),
            sequence_id: SequenceId(0),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        },
        turn_id: TurnId("1".to_owned()),
        text: "brief summary".to_owned(),
        signature: Some("opaque-encrypted-provider-payload".repeat(100)),
    };
    let public = serde_json::to_value(public_cli_event(event)).expect("public event");
    assert_eq!(public["text"], "brief summary");
    assert!(public["signature"].is_null());
    assert!(
        !public
            .to_string()
            .contains("opaque-encrypted-provider-payload")
    );
}

#[test]
fn protocol_interaction_queue_preserves_question_then_permission_order() {
    let mut interactions = VecDeque::from([
        PendingInteraction::Question {
            id: QuestionId("question-first".to_owned()),
            prompt: "first?".to_owned(),
            options: Vec::new(),
        },
        PendingInteraction::Permission {
            tool_call_id: "permission-second".to_owned(),
            invocation_id: rw_types::ToolInvocationId("permission-second-invocation".to_owned()),
            capabilities: vec![ToolCapability::ReadFilesystem],
            rationale: "fixture".to_owned(),
            binding: None,
        },
    ]);
    let Some(PendingInteraction::Question { id, .. }) = interactions.pop_front() else {
        panic!("question must remain first");
    };
    assert_eq!(id.0, "question-first");
    assert!(matches!(
        interactions.pop_front(),
        Some(PendingInteraction::Permission { tool_call_id, .. })
            if tool_call_id == "permission-second"
    ));
}
