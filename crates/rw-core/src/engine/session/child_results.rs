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
        self.undelivered.remove(&source);
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
    }

    /// Consumes a pending wake when undelivered results remain. The started
    /// turn owns delivery, so a failed turn never retriggers itself.
    pub(in crate::engine) fn take(&mut self) -> bool {
        let wake = self.requested && !self.undelivered.is_empty();
        self.requested = false;
        if wake {
            self.undelivered.clear();
        }
        wake
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

        wake.finished(SequenceId(9));
        wake.request(SequenceId(9));
        wake.cancel();
        assert!(!wake.take(), "an interrupt clears the pending wake");
    }
}
