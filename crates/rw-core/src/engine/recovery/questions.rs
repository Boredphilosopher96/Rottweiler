//! Answers consume exactly the source-qualified pending question they satisfy.
use super::{RecoveryError, RecoveryHead};
use rw_store::session::{SessionEventPageLimits, journal::JournalReadView};
use rw_types::{Answer, EngineEvent, QuestionId, SequenceId};

pub(super) fn answer(
    head: &mut RecoveryHead,
    source: &JournalReadView,
    turn: u64,
    question_id: &QuestionId,
    answer: &Answer,
) -> Result<(), RecoveryError> {
    let selected = head
        .control
        .questions
        .iter()
        .find(|entry| entry.id == question_id.0)
        .ok_or(RecoveryError::Invalid(
            "answer has no pending question source",
        ))?;
    let mut page = source.page::<EngineEvent>(
        selected.sequence.0.checked_sub(1).map(SequenceId),
        SessionEventPageLimits {
            max_page_events: 1,
            ..SessionEventPageLimits::default()
        },
    )?;
    let Some(EngineEvent::QuestionAsked {
        meta,
        question_id: asked,
        question,
        ..
    }) = page.events.pop().map(|entry| entry.event)
    else {
        return Err(RecoveryError::Invalid(
            "answer question source is unavailable",
        ));
    };
    if meta.sequence_id != selected.sequence
        || Some(&meta.session_id) != head.session_id.as_ref()
        || turn != selected.agent_turn
        || asked != *question_id
        || question.id != *question_id
    {
        return Err(RecoveryError::Invalid("answer source identity mismatch"));
    }
    rw_types::question_admission::validate_answer(&question, answer)
        .map_err(RecoveryError::Limit)?;
    head.control
        .questions
        .retain(|entry| entry.id != question_id.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::engine::{
        PendingEvent,
        recovery::{CanonicalRecovery, tests::append},
    };
    #[test]
    fn canonical_answer_requires_pending_source_identity_and_displayed_value() {
        for (value, id, turn, accepted) in [
            ("yes", "choice", 1, true),
            ("missing", "choice", 1, false),
            ("yes", "other", 1, false),
            ("yes", "choice", 2, false),
        ] {
            let root = tempfile::tempdir().expect("root");
            let mut log =
                rw_store::session::journal::SegmentedJournal::open(root.path(), "canonical")
                    .expect("source");
            let question_id = QuestionId("choice".into());
            append(
                &mut log,
                vec![
                    PendingEvent::TurnStarted { turn: 1 },
                    PendingEvent::QuestionAsked {
                        turn: 1,
                        question_id: question_id.clone(),
                        question: rw_types::Question {
                            id: question_id.clone(),
                            prompt: "Continue?".into(),
                            response_kind: rw_types::QuestionResponseKind::SelectOne,
                            options: vec![rw_types::QuestionOption {
                                value: "yes".into(),
                                label: "Yes".into(),
                                description: None,
                                model_context_transfer: None,
                            }],
                            model_switch: None,
                        },
                    },
                    PendingEvent::QuestionAnswered {
                        turn,
                        question_id,
                        answer: Answer {
                            question_id: QuestionId(id.into()),
                            value: value.into(),
                        },
                    },
                ],
            );
            let source = log.read_view();
            let modes = rw_ext::ModeRegistry::builtins().expect("modes");
            let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("index");
            assert_eq!(
                recovery.advance(&source, &modes).is_ok(),
                accepted,
                "value={value} id={id} turn={turn}"
            );
        }
    }
}
