//! Small live metadata independent of the conversation body and client replay cursor.
use rw_types::{
    EngineEvent, SequenceId, TurnId,
    session_state::{SessionBudgetState, SessionCompactionState},
};

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
                self.turn_source = Some((turn_id.clone(), meta.sequence_id))
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
    pub(in crate::engine) fn compaction_attempt(&mut self, turn: u64, attempt: u32) {
        if let Some(compaction) = &mut self.compaction
            && compaction.summary_turn_id == crate::engine::wire_turn_id(turn)
        {
            compaction.attempt = Some(attempt);
        }
    }
}
