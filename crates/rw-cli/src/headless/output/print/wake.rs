//! A one-shot run stays open while background children can still report.
//!
//! After the prompt's turn completes, the run keeps consuming the session while
//! any child runs or waits for a slot, and while a finished result has not yet
//! reached the model. Each result wakes the idle parent for a turn whose output
//! is printed as part of the run. The run ends once nothing can wake it again.
use super::{
    PrintInterrupts, PrintOutput, Terminal, answer_noninteractive, display_agent_error,
    event_message, write,
};
use crate::cli_args::OutputFormat;
use miette::{Result, miette};
use rw_core::EngineEvent;
use rw_runtime::session::WakingChildren;
use rw_types::{SequenceId, conversation_input::ContextSelection};
use std::collections::BTreeSet;

/// Turns a single run may start for child results before it stops waiting.
/// Each wake turn can start more children, so this bounds the whole run.
pub(super) const MAX_WAKE_TURNS: usize = 64;

/// What the event log says about results the parent has not yet received.
#[derive(Debug, Default)]
pub(super) struct WakeTracker {
    /// `subagent_finished` events whose result no provider call has committed.
    undelivered: BTreeSet<SequenceId>,
    /// Sequence of the running turn's `turn_started`.
    running: Option<SequenceId>,
    /// Highest durable sequence observed on the subscription.
    seen: Option<SequenceId>,
    wake_turns: usize,
    abandoned: bool,
}

impl WakeTracker {
    pub(super) fn observe(&mut self, event: &EngineEvent) {
        let sequence = event.meta().map(|meta| meta.sequence_id);
        if let Some(sequence) = sequence {
            self.seen = self.seen.max(Some(sequence));
        }
        match event {
            EngineEvent::SubagentFinished { .. } => {
                if let Some(sequence) = sequence {
                    self.undelivered.insert(sequence);
                }
            }
            EngineEvent::ConversationContextCommitted {
                selection: ContextSelection::ChildResult { source },
                ..
            } => {
                self.undelivered.remove(source);
            }
            EngineEvent::TurnStarted { .. } => {
                self.running = sequence.or(Some(SequenceId(0)));
            }
            EngineEvent::TurnFinished { .. } => {
                // A turn owns every result that was waiting when it started: it
                // delivers them or, if it stops early, they never wake it again.
                // Results finished during the turn wake the parent afterwards.
                if let Some(started) = self.running.take() {
                    self.undelivered = self.undelivered.split_off(&started);
                }
            }
            // An idle parent that reports an error could not start its wake turn.
            EngineEvent::Error { .. } if self.running.is_none() => self.undelivered.clear(),
            _ => {}
        }
    }

    /// A wake turn started; returns false once the run has used its allowance.
    pub(super) fn count_wake_turn(&mut self) -> bool {
        self.wake_turns += 1;
        self.wake_turns <= MAX_WAKE_TURNS
    }

    /// Stops waiting for children; only a running turn is still followed.
    pub(super) fn abandon(&mut self) {
        self.abandoned = true;
        self.undelivered.clear();
    }

    pub(super) const fn abandoned(&self) -> bool {
        self.abandoned
    }

    pub(super) const fn turn_running(&self) -> bool {
        self.running.is_some()
    }

    /// No turn runs and every observed result has reached the model.
    pub(super) fn idle(&self) -> bool {
        self.running.is_none() && self.undelivered.is_empty()
    }

    /// Whether the subscription has delivered every durable event up to `tail`.
    pub(super) fn caught_up(&self, tail: Option<SequenceId>) -> bool {
        tail.is_none_or(|tail| self.seen.is_some_and(|seen| seen >= tail))
    }
}

pub(super) struct WakeRun<'a> {
    pub(super) actor: &'a rw_core::SessionHandle,
    pub(super) children: &'a WakingChildren,
    pub(super) events: &'a mut rw_core::SessionSubscription,
    pub(super) format: OutputFormat,
    pub(super) printer: &'a mut Terminal,
    pub(super) interrupts: &'a mut PrintInterrupts,
    pub(super) aggregate: &'a mut PrintOutput,
}

impl WakeRun<'_> {
    /// Follows wake turns until no child can report to the parent again.
    pub(super) async fn follow(self, mut tracker: WakeTracker) -> Result<()> {
        loop {
            let outstanding = !tracker.abandoned() && self.children.outstanding();
            if tracker.idle() && !outstanding {
                // Children stop counting only after their result is durable,
                // so reading to the current tail observes every result.
                let tail = self
                    .actor
                    .last_sequence()
                    .await
                    .map_err(display_agent_error)?;
                if tracker.caught_up(tail) {
                    return Ok(());
                }
            }
            let event = tokio::select! {
                event = self.events.recv() => {
                    event.map_err(|error| miette!("session event stream failed: {error}"))?
                }
                () = self.children.settled(), if outstanding => continue,
                signal = self.interrupts.recv() => {
                    signal?;
                    tracker.abandon();
                    if tracker.turn_running() {
                        self.actor.interrupt().await.map_err(display_agent_error)?;
                    }
                    continue;
                }
            };
            let event = event.as_ref();
            tracker.observe(event);
            if matches!(event, EngineEvent::TurnStarted { .. }) && !tracker.count_wake_turn() {
                tracker.abandon();
                write(
                    self.actor,
                    self.printer,
                    self.interrupts,
                    format!(
                        "stopped waiting for child agents after {MAX_WAKE_TURNS} turns started by their results\n"
                    ),
                    true,
                )
                .await?;
            }
            answer_noninteractive(self.actor, event).await?;
            if let Some((message, stderr)) = event_message(event, self.format)? {
                write(self.actor, self.printer, self.interrupts, message, stderr).await?;
            }
            self.aggregate.push(event)?;
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use rw_types::{
        Cost, EventMeta, SessionId, SubagentId, SubagentResult, SubagentStatus, TurnId, TurnStatus,
        Usage,
    };

    fn meta(sequence: u64) -> EventMeta {
        EventMeta {
            protocol_version: 1,
            session_id: SessionId("parent".into()),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-01-01T00:00:00Z".into(),
            caused_by: None,
        }
    }

    fn usage() -> Usage {
        Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        }
    }

    fn started(sequence: u64) -> EngineEvent {
        EngineEvent::TurnStarted {
            meta: meta(sequence),
            turn_id: TurnId("1".into()),
        }
    }

    fn finished_turn(sequence: u64, status: TurnStatus) -> EngineEvent {
        EngineEvent::TurnFinished {
            meta: meta(sequence),
            turn_id: TurnId("1".into()),
            status,
            usage: usage(),
            cost: Cost::Unavailable {
                reason: "test".into(),
            },
        }
    }

    fn child_finished(sequence: u64) -> EngineEvent {
        let id = SubagentId(format!("agent-{sequence}"));
        EngineEvent::SubagentFinished {
            meta: meta(sequence),
            subagent_id: id.clone(),
            result: SubagentResult {
                subagent_id: id,
                session_id: SessionId("child".into()),
                status: SubagentStatus::Completed,
                final_text: "done".into(),
                touched_files: Vec::new(),
                diff_artifact: None,
                usage: usage(),
                cost: Cost::Unavailable {
                    reason: "test".into(),
                },
                turns: 1,
                duration_millis: 1,
            },
        }
    }

    fn delivered(sequence: u64, source: u64) -> EngineEvent {
        EngineEvent::ConversationContextCommitted {
            meta: meta(sequence),
            agent_turn: 2,
            selection: ContextSelection::ChildResult {
                source: SequenceId(source),
            },
        }
    }

    #[test]
    fn an_undelivered_result_keeps_the_run_open_until_a_turn_delivers_it() {
        let mut tracker = WakeTracker::default();
        tracker.observe(&started(1));
        tracker.observe(&finished_turn(2, TurnStatus::Completed));
        assert!(tracker.idle());
        tracker.observe(&child_finished(3));
        assert!(!tracker.idle(), "the idle parent will wake for this result");
        assert!(!tracker.caught_up(Some(SequenceId(4))));
        tracker.observe(&started(4));
        tracker.observe(&delivered(5, 3));
        assert!(!tracker.idle(), "the wake turn is still running");
        tracker.observe(&finished_turn(6, TurnStatus::Completed));
        assert!(tracker.idle());
        assert!(tracker.caught_up(Some(SequenceId(6))));
    }

    #[test]
    fn a_turn_owns_results_waiting_at_its_start_but_not_later_ones() {
        let mut tracker = WakeTracker::default();
        tracker.observe(&child_finished(3));
        tracker.observe(&child_finished(4));
        tracker.observe(&started(5));
        tracker.observe(&child_finished(7));
        tracker.observe(&finished_turn(8, TurnStatus::Failed));
        assert!(
            !tracker.idle(),
            "a result finished during the turn wakes the parent afterwards"
        );
        tracker.observe(&started(9));
        tracker.observe(&finished_turn(10, TurnStatus::Completed));
        assert!(
            tracker.idle(),
            "a result the wake turn left behind never rewakes"
        );
    }

    #[test]
    fn an_idle_error_ends_the_wait_and_interrupts_abandon_children() {
        let mut tracker = WakeTracker::default();
        tracker.observe(&child_finished(3));
        tracker.observe(&EngineEvent::Error {
            meta: meta(4),
            error: rw_types::EngineError {
                category: rw_types::EngineErrorCategory::Config,
                code: "turn_unavailable".into(),
                message: "no turn could start".into(),
                retryable: false,
                details: None,
            },
        });
        assert!(tracker.idle());
        tracker.observe(&child_finished(5));
        tracker.abandon();
        assert!(tracker.idle() && tracker.abandoned());
    }

    #[test]
    fn wake_turns_are_bounded() {
        let mut tracker = WakeTracker::default();
        for _ in 0..MAX_WAKE_TURNS {
            assert!(tracker.count_wake_turn());
        }
        assert!(!tracker.count_wake_turn());
    }
}
