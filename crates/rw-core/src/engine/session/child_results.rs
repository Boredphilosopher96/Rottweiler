//! Live wake bookkeeping for finished child results the parent has not received.
//!
//! The durable record of delivery is the canonical history; this state only
//! decides whether an idle parent should start a turn. It starts empty after
//! recovery, so a restart never starts an unsolicited turn: results pending
//! from before the restart are delivered with the next turn.
use rw_types::SequenceId;
use std::collections::BTreeSet;

/// Bound matching the canonical undelivered selector list.
const MAX_TRACKED: usize = crate::engine::recovery::MAX_UNDELIVERED_CHILD_RESULTS;

#[derive(Debug, Default)]
pub(in crate::engine) struct ChildResultWake {
    undelivered: BTreeSet<SequenceId>,
    requested: bool,
    /// A wake turn is running; `progressed` records whether it delivered any
    /// result, which is what allows another wake for the remainder.
    waking: bool,
    progressed: bool,
}

impl ChildResultWake {
    /// A `subagent_finished` event was committed at `source`.
    pub(in crate::engine) fn finished(&mut self, source: SequenceId) {
        if self.undelivered.len() == MAX_TRACKED {
            self.undelivered.pop_first();
        }
        self.undelivered.insert(source);
    }

    /// A provider call committed the result at `source`.
    pub(in crate::engine) fn delivered(&mut self, source: SequenceId) {
        if self.undelivered.remove(&source) && self.waking {
            self.progressed = true;
        }
    }

    /// A background child finished with nobody waiting on its result.
    pub(in crate::engine) fn request(&mut self, source: SequenceId) {
        if self.undelivered.contains(&source) {
            self.requested = true;
        }
    }

    /// The user stopped the parent; later completions may wake it again.
    pub(in crate::engine) fn cancel(&mut self) {
        self.requested = false;
        self.waking = false;
        self.progressed = false;
    }

    /// Consumes a pending wake when undelivered results remain. The started
    /// turn owns delivery of as many results as one provider call accepts.
    pub(in crate::engine) fn take(&mut self) -> bool {
        let wake = self.requested && !self.undelivered.is_empty();
        self.requested = false;
        if wake {
            self.waking = true;
            self.progressed = false;
        }
        wake
    }

    /// A turn ended without interruption. A wake turn that delivered at least
    /// one result wakes again for any that did not fit, so every finished
    /// child reaches the parent; a wake turn that delivered nothing never
    /// retriggers itself.
    pub(in crate::engine) fn turn_ended(&mut self) {
        if std::mem::take(&mut self.waking)
            && std::mem::take(&mut self.progressed)
            && !self.undelivered.is_empty()
        {
            self.requested = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_once_only_for_results_still_undelivered() {
        let mut wake = ChildResultWake::default();
        wake.finished(SequenceId(4));
        wake.delivered(SequenceId(4));
        wake.request(SequenceId(4));
        assert!(!wake.take(), "an already delivered result never wakes");

        wake.finished(SequenceId(7));
        wake.request(SequenceId(7));
        assert!(wake.take());
        assert!(!wake.take(), "the started turn owns delivery");

        wake.turn_ended();
        assert!(
            !wake.take(),
            "a wake turn that delivered nothing never retriggers"
        );

        wake.finished(SequenceId(9));
        wake.request(SequenceId(9));
        wake.cancel();
        assert!(!wake.take(), "an interrupt clears the pending wake");
    }

    #[test]
    fn wakes_again_until_every_result_beyond_one_batch_is_delivered() {
        let mut wake = ChildResultWake::default();
        for source in 1..=10 {
            wake.finished(SequenceId(source));
            wake.request(SequenceId(source));
        }
        assert!(wake.take());
        for source in 1..=8 {
            wake.delivered(SequenceId(source));
        }
        wake.turn_ended();
        assert!(wake.take(), "two results did not fit the first wake turn");
        wake.delivered(SequenceId(9));
        wake.delivered(SequenceId(10));
        wake.turn_ended();
        assert!(!wake.take(), "nothing remains to deliver");
    }
}
