//! Admission for canonical unresolved question payloads.
use crate::{Question, allocation::PrepareAllocation};

pub const MAX_PENDING_QUESTION_REQUESTS: usize = 64;
pub const MAX_QUESTION_BYTES: usize = 64 * 1024;
pub const MAX_QUESTION_PREPARED_BYTES: usize = 256 * 1024;
pub const MAX_PENDING_QUESTION_BYTES: usize = MAX_PENDING_QUESTION_REQUESTS * MAX_QUESTION_BYTES;
pub const MAX_PENDING_QUESTION_PREPARED_BYTES: usize =
    MAX_PENDING_QUESTION_REQUESTS * MAX_QUESTION_PREPARED_BYTES;

/// Validate one request before copying or announcing its retained wire payload.
/// # Errors
/// Rejects invalid identity, unsupported input shape and allocation overflow.
pub fn validate_question(question: &Question) -> Result<(), &'static str> {
    if question.id.0.is_empty() || question.id.0.len() > 256 {
        return Err("question identity must be bounded and nonempty");
    }
    if question
        .prepared_bytes()
        .is_none_or(|bytes| bytes > MAX_QUESTION_PREPARED_BYTES)
    {
        return Err("question prepared allocation exceeds admission");
    }
    match &question.response_kind {
        crate::QuestionResponseKind::Text
            if !question.options.is_empty() || question.model_switch.is_some() =>
        {
            return Err("text question cannot declare selection options");
        }
        crate::QuestionResponseKind::SelectOne if question.options.is_empty() => {
            return Err("selection question requires options");
        }
        _ => {}
    }
    let mut values = std::collections::BTreeSet::new();
    for option in &question.options {
        if option.value.is_empty() || !values.insert(&option.value) {
            return Err("question option values must be nonempty and unique");
        }
    }
    crate::json_encoding::JsonWriter::count(MAX_QUESTION_BYTES)
        .serialize(question)
        .map_err(|_| "question serialized payload exceeds admission")
}
/// # Errors
/// The sole answer must identify the pending question and satisfy its input kind.
pub fn validate_answer(question: &Question, answer: &crate::Answer) -> Result<(), &'static str> {
    if answer.question_id != question.id || answer.value.is_empty() {
        return Err("answer must identify the pending question and contain a value");
    }
    if answer.value.len() > MAX_QUESTION_BYTES {
        return Err("answer exceeds byte admission");
    }
    if question.response_kind == crate::QuestionResponseKind::SelectOne
        && !question
            .options
            .iter()
            .any(|option| option.value == answer.value)
    {
        return Err("answer must select a displayed option");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    fn question() -> Question {
        Question {
            id: crate::QuestionId("choice".into()),
            prompt: "Choose".into(),
            response_kind: crate::QuestionResponseKind::Text,
            options: vec![],
            model_switch: None,
        }
    }
    #[test]
    fn question_admission_charges_escaped_text_and_retained_capacity() {
        assert!(validate_question(&question()).is_ok());
        let mut value = question();
        value.prompt = "\0".repeat(MAX_QUESTION_BYTES / 2);
        assert!(validate_question(&value).is_err());
        value.prompt = String::with_capacity(MAX_QUESTION_PREPARED_BYTES + 1);
        assert!(validate_question(&value).is_err());
    }
    #[test]
    fn answers_require_exact_identity_and_displayed_selection() {
        let mut question = question();
        let mut answer = crate::Answer {
            question_id: question.id.clone(),
            value: "yes".into(),
        };
        assert!(validate_answer(&question, &answer).is_ok());
        answer.question_id.0 = "foreign".into();
        assert!(validate_answer(&question, &answer).is_err());
        answer.question_id = question.id.clone();
        question.response_kind = crate::QuestionResponseKind::SelectOne;
        assert!(validate_question(&question).is_err());
        question.options.push(crate::QuestionOption {
            value: "no".into(),
            label: "No".into(),
            description: None,
            model_context_transfer: None,
        });
        assert!(validate_question(&question).is_ok());
        assert!(validate_answer(&question, &answer).is_err());
        answer.value = "no".into();
        assert!(validate_answer(&question, &answer).is_ok());
        answer.value.clear();
        assert!(validate_answer(&question, &answer).is_err());
    }
    #[test]
    fn singular_question_and_answer_reject_unsupported_payloads() {
        let mut question = serde_json::to_value(question()).expect("question");
        question["response_kind"] = serde_json::json!("select_many");
        assert!(serde_json::from_value::<Question>(question).is_err());
        assert!(
            serde_json::from_value::<crate::Answer>(serde_json::json!({
                "question_id":"choice", "values":["yes","no"]
            }))
            .is_err()
        );
        let session = serde_json::json!({"question_id":"choice", "turn_id":"1", "questions":[]});
        assert!(
            serde_json::from_value::<crate::session_controls::SessionQuestion>(session).is_err()
        );
    }
}
