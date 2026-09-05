//! Admission for canonical unresolved question payloads.
use crate::{Question, allocation::PrepareAllocation};
use std::io::{self, Write};

pub const MAX_PENDING_QUESTION_REQUESTS: usize = 64;
pub const MAX_QUESTION_SET_BYTES: usize = 64 * 1024;
pub const MAX_QUESTION_SET_PREPARED_BYTES: usize = 256 * 1024;
pub const MAX_PENDING_QUESTION_BYTES: usize =
    MAX_PENDING_QUESTION_REQUESTS * MAX_QUESTION_SET_BYTES;
pub const MAX_PENDING_QUESTION_PREPARED_BYTES: usize =
    MAX_PENDING_QUESTION_REQUESTS * MAX_QUESTION_SET_PREPARED_BYTES;

/// Validate a request before copying or announcing its questions. The original
/// producer owns its input allocation; this checks the exact retained wire payload.
/// # Errors
/// Rejects empty/oversized question sets, duplicate identities and byte overflow.
pub fn validate_questions(questions: &Vec<Question>) -> Result<(), &'static str> {
    if questions.is_empty() || questions.len() > MAX_PENDING_QUESTION_REQUESTS {
        return Err("question entry count exceeds admission");
    }
    if questions
        .prepared_bytes()
        .is_none_or(|bytes| bytes > MAX_QUESTION_SET_PREPARED_BYTES)
    {
        return Err("question prepared allocation exceeds admission");
    }
    let mut identities = std::collections::BTreeSet::new();
    for question in questions {
        if question.id.0.is_empty()
            || question.id.0.len() > 256
            || !identities.insert(&question.id.0)
        {
            return Err("question identities must be bounded and unique");
        }
    }
    serde_json::to_writer(LimitedSize(0), questions)
        .map_err(|_| "question serialized payload exceeds admission")
}

struct LimitedSize(usize);
impl Write for LimitedSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|bytes| *bytes <= MAX_QUESTION_SET_BYTES)
            .ok_or_else(|| io::Error::other("question byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn question() -> Question {
        Question {
            id: crate::QuestionId("choice".into()),
            prompt: "Choose a value".into(),
            response_kind: crate::QuestionResponseKind::Text,
            options: Vec::new(),
            model_switch: None,
        }
    }
    #[test]
    fn question_admission_charges_serialization_expansion_and_spare_capacity() {
        assert!(validate_questions(&vec![question()]).is_ok());
        let mut escaped = question();
        escaped.prompt = "\0".repeat(MAX_QUESTION_SET_BYTES / 2);
        assert!(validate_questions(&vec![escaped]).is_err());
        let mut reserved = Vec::with_capacity(
            MAX_QUESTION_SET_PREPARED_BYTES / std::mem::size_of::<Question>() + 1,
        );
        reserved.push(question());
        assert!(validate_questions(&reserved).is_err());
    }
    #[test]
    fn question_admission_rejects_duplicate_or_missing_identities() {
        assert!(validate_questions(&vec![question(), question()]).is_err());
        assert!(validate_questions(&Vec::new()).is_err());
        let mut missing = question();
        missing.id.0.clear();
        assert!(validate_questions(&vec![missing]).is_err());
    }
}
