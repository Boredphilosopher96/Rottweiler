//! Small live metadata independent of the conversation body and client replay cursor.
use crate::engine::AgentLoopError;
use rw_types::transcript_tail::{TRANSCRIPT_TAIL_TEXT_BYTES, TranscriptTailText};
use rw_types::{
    EngineEvent, SequenceId, TurnId,
    session_state::{SessionBudgetState, SessionCompactionState},
};

#[derive(Clone, Copy)]
pub(in crate::engine) enum CompactionPreview<'a> {
    Started,
    Text(&'a str),
    Thinking(&'a str),
}

#[derive(Default)]
pub(in crate::engine) struct LiveState {
    pub(in crate::engine) controls_source: Option<SequenceId>,
    pub(in crate::engine) turn_source: Option<(TurnId, SequenceId)>,
    pub(in crate::engine) compaction: Option<SessionCompactionState>,
    pub(in crate::engine) budget: Option<SessionBudgetState>,
    pub(in crate::engine) plugin_statuses: Vec<rw_types::session_state::SessionPluginStatus>,
}
impl LiveState {
    pub(in crate::engine) fn admit_statuses(
        &self,
        kinds: &[crate::engine::PendingEvent],
    ) -> Result<(), AgentLoopError> {
        if !kinds.iter().any(|kind| {
            matches!(
                kind,
                crate::engine::PendingEvent::PluginStatusChanged { .. }
            )
        }) {
            return Ok(());
        }
        let mut identities: std::collections::BTreeSet<&str> = self
            .plugin_statuses
            .iter()
            .map(|entry| entry.plugin_id.as_str())
            .collect();
        for kind in kinds {
            if let crate::engine::PendingEvent::PluginStatusChanged { plugin_id, status } = kind {
                rw_types::session_state::validate_plugin_status(plugin_id, status)
                    .map_err(|message| AgentLoopError::InvalidConfiguration(message.into()))?;
                if status.is_empty() {
                    identities.remove(plugin_id.as_str());
                } else {
                    identities.insert(plugin_id);
                    if identities.len() > rw_types::session_state::MAX_SESSION_PLUGIN_STATUSES {
                        return Err(AgentLoopError::InvalidConfiguration(
                            "active plugin status limit reached".into(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    pub(in crate::engine) fn observe(&mut self, event: &EngineEvent, running: Option<u64>) {
        if super::control_observation::is_control_event(event) {
            self.controls_source = event.meta().map(|meta| meta.sequence_id);
        }
        match event {
            EngineEvent::PluginStatusChanged {
                meta,
                plugin_id,
                status,
            } => {
                self.plugin_statuses
                    .retain(|entry| entry.plugin_id != *plugin_id);
                if !status.is_empty() {
                    self.plugin_statuses
                        .push(rw_types::session_state::SessionPluginStatus {
                            plugin_id: plugin_id.clone(),
                            status: status.clone(),
                            source: meta.sequence_id,
                        });
                }
            }
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

    #[test]
    fn status_admission_tracks_batch_clears_before_durable_append() {
        use crate::engine::PendingEvent;
        use rw_types::session_state::{
            MAX_PLUGIN_STATUS_BYTES, MAX_SESSION_PLUGIN_STATUSES, SessionPluginStatus,
        };
        let live = LiveState {
            plugin_statuses: (0..MAX_SESSION_PLUGIN_STATUSES)
                .map(|index| SessionPluginStatus {
                    plugin_id: format!("plugin-{index}"),
                    status: "ready".into(),
                    source: SequenceId(index as u64),
                })
                .collect(),
            ..LiveState::default()
        };
        let update = |id: &str, status: String| PendingEvent::PluginStatusChanged {
            plugin_id: id.into(),
            status,
        };
        assert!(
            live.admit_statuses(&[update("overflow", "ready".into())])
                .is_err()
        );
        assert!(
            live.admit_statuses(&[update("plugin-0", "updated".into())])
                .is_ok()
        );
        assert!(
            live.admit_statuses(&[
                update("plugin-0", String::new()),
                update("replacement", "ready".into())
            ])
            .is_ok()
        );
        assert!(
            live.admit_statuses(&[update(
                "plugin-0",
                "€".repeat(MAX_PLUGIN_STATUS_BYTES / 3 + 1)
            )])
            .is_err()
        );
        assert!(
            live.admit_statuses(&[update("plugin-0", "bad\nstatus".into())])
                .is_err()
        );
        assert_eq!(live.plugin_statuses.len(), MAX_SESSION_PLUGIN_STATUSES);
    }

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
