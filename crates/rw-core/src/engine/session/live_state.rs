//! Small live metadata independent of the conversation body and client replay cursor.
use crate::engine::AgentLoopError;
use rw_types::transcript_tail::{TRANSCRIPT_TAIL_TEXT_BYTES, TranscriptTailText};
use rw_types::{
    EngineEvent, SequenceId, TurnId,
    session_state::{SessionBudgetState, SessionCompactionState},
};

pub(in crate::engine) enum CompactionPreview<'a> {
    Started,
    Text(&'a str),
    Thinking(&'a str),
}

#[derive(Default)]
pub(in crate::engine) struct LiveState {
    pub(in crate::engine) turn_source: Option<(TurnId, SequenceId)>,
    pub(in crate::engine) compaction: Option<SessionCompactionState>,
    pub(in crate::engine) budget: Option<SessionBudgetState>,
}
impl LiveState {
    pub(in crate::engine) fn observe(&mut self, event: &EngineEvent, running: Option<u64>) {
        match event {
            EngineEvent::TurnStarted { meta, turn_id } => {
                self.turn_source = Some((turn_id.clone(), meta.sequence_id));
            }
            EngineEvent::TurnFinished { turn_id, .. } => {
                if self
                    .turn_source
                    .as_ref()
                    .is_some_and(|(active, _)| active == turn_id)
                {
                    self.turn_source = None;
                }
            }
            EngineEvent::CompactionStarted { meta, .. } => {
                self.compaction = running.map(|turn| SessionCompactionState {
                    summary_turn_id: crate::engine::wire_turn_id(turn),
                    started: meta.sequence_id,
                    attempt: None,
                    revision: 0,
                    text: empty_preview(),
                    thinking: empty_preview(),
                });
            }
            EngineEvent::CompactionFinished { .. }
            | EngineEvent::CompactionFailed { .. }
            | EngineEvent::Error { .. } => self.compaction = None,
            EngineEvent::ConversationRewound { .. } => {
                self.turn_source = None;
                self.compaction = None;
            }
            EngineEvent::BudgetStatusChanged {
                turn_id,
                level,
                scope,
                unit,
                current,
                limit,
                ..
            } => {
                self.budget = Some(SessionBudgetState {
                    turn_id: turn_id.clone(),
                    level: level.clone(),
                    scope: scope.clone(),
                    unit: unit.clone(),
                    current: *current,
                    limit: *limit,
                });
            }
            _ => {}
        }
    }
    pub(in crate::engine) fn compaction_progress(
        &mut self,
        turn: u64,
        attempt: u32,
        update: CompactionPreview<'_>,
    ) -> Result<Option<(SequenceId, u64)>, AgentLoopError> {
        let Some(compaction) = &mut self.compaction else {
            return Ok(None);
        };
        if compaction.summary_turn_id != crate::engine::wire_turn_id(turn)
            || compaction.attempt.is_some_and(|current| attempt < current)
        {
            return Ok(None);
        }
        let revision = compaction.revision.checked_add(1).ok_or_else(|| {
            AgentLoopError::InvalidConfiguration("compaction display revision exhausted".into())
        })?;
        if compaction.attempt != Some(attempt) {
            compaction.text = empty_preview();
            compaction.thinking = empty_preview();
        }
        compaction.attempt = Some(attempt);
        compaction.revision = revision;
        match update {
            CompactionPreview::Started => {}
            CompactionPreview::Text(text) => append_preview(&mut compaction.text, text),
            CompactionPreview::Thinking(text) => append_preview(&mut compaction.thinking, text),
        }
        Ok(Some((compaction.started, revision)))
    }
}
fn empty_preview() -> TranscriptTailText {
    TranscriptTailText {
        text: String::new(),
        truncated: false,
    }
}
fn append_preview(preview: &mut TranscriptTailText, text: &str) {
    if preview.truncated {
        return;
    }
    let remaining = TRANSCRIPT_TAIL_TEXT_BYTES.saturating_sub(preview.text.len());
    let end = text.floor_char_boundary(remaining.min(text.len()));
    preview.text.push_str(&text[..end]);
    preview.truncated = end < text.len();
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn active() -> LiveState {
        LiveState {
            compaction: Some(SessionCompactionState {
                summary_turn_id: crate::engine::wire_turn_id(1),
                started: SequenceId(10),
                attempt: None,
                revision: 0,
                text: empty_preview(),
                thinking: empty_preview(),
            }),
            ..LiveState::default()
        }
    }

    #[test]
    fn compaction_snapshot_previews_are_bounded_and_revision_fenced_across_attempts() {
        let mut live = active();
        let input = "€".repeat(TRANSCRIPT_TAIL_TEXT_BYTES);
        assert_eq!(
            live.compaction_progress(1, 0, CompactionPreview::Started)
                .expect("start"),
            Some((SequenceId(10), 1))
        );
        live.compaction_progress(1, 0, CompactionPreview::Text(&input))
            .expect("text");
        live.compaction_progress(1, 0, CompactionPreview::Thinking(&input))
            .expect("thinking");
        let snapshot = live.compaction.clone().expect("snapshot");
        assert_eq!(snapshot.revision, 3);
        assert_eq!(snapshot.text.text.len(), TRANSCRIPT_TAIL_TEXT_BYTES / 3 * 3);
        assert!(snapshot.text.truncated && snapshot.thinking.truncated);
        live.compaction_progress(
            1,
            0,
            CompactionPreview::Text("must not bridge omitted text"),
        )
        .expect("bounded");
        assert_eq!(
            live.compaction.as_ref().expect("active").text,
            snapshot.text
        );
        live.compaction_progress(1, 1, CompactionPreview::Started)
            .expect("next attempt");
        assert_eq!(live.compaction.as_ref().expect("active").revision, 5);
        assert!(
            live.compaction
                .as_ref()
                .expect("active")
                .text
                .text
                .is_empty()
        );
        assert!(!live.compaction.as_ref().expect("active").text.truncated);
        assert_eq!(
            live.compaction_progress(1, 0, CompactionPreview::Text("stale"))
                .expect("stale"),
            None
        );
        assert_eq!(
            live.compaction_progress(2, 1, CompactionPreview::Text("foreign"))
                .expect("foreign"),
            None
        );
        assert_eq!(live.compaction.as_ref().expect("active").revision, 5);
    }

    #[test]
    fn closed_compaction_and_exhausted_revisions_never_publish_an_ambiguous_update() {
        let mut live = LiveState::default();
        assert_eq!(
            live.compaction_progress(1, 0, CompactionPreview::Text("closed"))
                .expect("closed"),
            None
        );
        live = active();
        live.compaction.as_mut().expect("active").revision = u64::MAX;
        assert!(
            live.compaction_progress(1, 0, CompactionPreview::Text("overflow"))
                .is_err()
        );
        assert!(
            live.compaction
                .as_ref()
                .expect("active")
                .text
                .text
                .is_empty()
        );
    }
}
