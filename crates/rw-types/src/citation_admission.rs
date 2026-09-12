//! Aggregate admission for one agent turn's citation announcements.
use serde::{Deserialize, Serialize};

pub const MAX_TURN_CITATIONS: usize = 256;
pub const MAX_CITATION_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_TURN_CITATION_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_TURN_CITATION_PREPARED_BYTES: usize = 4 * 1024 * 1024;

/// Separate instances charge announced deltas and committed IR. A committed copy
/// is not another announcement; both representations must fit the same ceiling.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CitationAdmission {
    count: usize,
    text_bytes: usize,
    prepared_bytes: usize,
}
impl CitationAdmission {
    /// Charges the actual retained strings, including spare producer capacity.
    /// Rejection leaves the prior allowance unchanged.
    /// # Errors
    /// Rejects an oversized entry, aggregate or arithmetic overflow.
    pub fn admit(
        &mut self,
        uri: &String,
        title: Option<&String>,
        excerpt: Option<&String>,
    ) -> Result<(), &'static str> {
        let mut text = uri.len();
        let mut prepared = uri.capacity();
        for value in [title, excerpt].into_iter().flatten() {
            text = text
                .checked_add(value.len())
                .ok_or("citation size overflow")?;
            prepared = prepared
                .checked_add(value.capacity())
                .ok_or("citation size overflow")?;
        }
        if text > MAX_CITATION_TEXT_BYTES {
            return Err("citation text exceeds admission");
        }
        // Covers each canonical block and its live event's string headers.
        prepared = prepared
            .checked_add(text)
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<crate::Block>()))
            .and_then(|bytes| bytes.checked_add(3 * std::mem::size_of::<String>()))
            .ok_or("citation size overflow")?;
        let next = Self {
            count: self.count.checked_add(1).ok_or("citation count overflow")?,
            text_bytes: self
                .text_bytes
                .checked_add(text)
                .ok_or("citation size overflow")?,
            prepared_bytes: self
                .prepared_bytes
                .checked_add(prepared)
                .ok_or("citation size overflow")?,
        };
        if next.count > MAX_TURN_CITATIONS
            || next.text_bytes > MAX_TURN_CITATION_TEXT_BYTES
            || next.prepared_bytes > MAX_TURN_CITATION_PREPARED_BYTES
        {
            return Err("agent turn citations exceed aggregate admission");
        }
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{CitationAdmission, MAX_TURN_CITATION_PREPARED_BYTES, MAX_TURN_CITATIONS};
    #[test]
    fn failed_citation_admission_does_not_consume_allowance() {
        let mut budget = CitationAdmission::default();
        let mut reserved = String::with_capacity(MAX_TURN_CITATION_PREPARED_BYTES);
        reserved.push('a');
        assert!(budget.admit(&reserved, None, None).is_err());
        assert_eq!(budget, CitationAdmission::default());
        for _ in 0..MAX_TURN_CITATIONS {
            budget
                .admit(&String::from("https://example.test"), None, None)
                .expect("within limit");
        }
        let full = budget;
        assert!(
            budget
                .admit(&String::from("https://example.test"), None, None)
                .is_err()
        );
        assert_eq!(budget, full);
    }
    #[test]
    fn citation_text_is_charged_across_fields_and_entries() {
        let mut budget = CitationAdmission::default();
        let half = "u".repeat(super::MAX_CITATION_TEXT_BYTES / 2);
        assert!(
            budget
                .admit(&half, Some(&half), Some(&String::from("x")))
                .is_err()
        );
        for _ in 0..super::MAX_TURN_CITATION_TEXT_BYTES / half.len() {
            budget.admit(&half, None, None).expect("aggregate boundary");
        }
        assert!(budget.admit(&String::from("x"), None, None).is_err());
    }
}
