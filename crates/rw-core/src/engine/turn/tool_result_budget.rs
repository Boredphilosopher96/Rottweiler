//! One family-charged owner spans tool callbacks, ordered results, and provider IR.
use super::tool_requests::ToolExecution;
use crate::engine::{AgentLoopError, SessionActorConfig, recovery::HistoryWorkingAllowance};
use rw_types::{ToolOutput, allocation::PrepareAllocation};
use std::sync::{Arc, Mutex};

pub(super) const REJECTED_OUTPUT: &str = "tool output rejected: aggregate result byte or structural admission exhausted; tool effects remain recorded";
const SLOT_BYTES: usize = 4096;
const SCRATCH_BYTES: usize = 2 * rw_types::tool_result_admission::MAX_TOOL_RESULT_IR_BYTES;
const OUTPUT_BYTES: usize = crate::engine::recovery::MAX_HISTORY_RESULT_BYTES - SCRATCH_BYTES;

#[derive(Clone)]
pub(in crate::engine) struct ToolResultBudget(Arc<Mutex<State>>);
struct State {
    owner: Box<dyn HistoryWorkingAllowance>,
    slots: Vec<usize>,
    retained: usize,
}
impl ToolResultBudget {
    pub(super) async fn new(
        config: &SessionActorConfig,
        slots: usize,
    ) -> Result<Self, AgentLoopError> {
        if slots == 0 || slots > rw_types::tool_admission::MAX_PENDING_TOOL_INVOCATIONS {
            return Err(invalid());
        }
        let mut owner = super::history_context::reserve_working(config).await?;
        let retained = slots * SLOT_BYTES;
        owner.resize(retained + SCRATCH_BYTES)?;
        Ok(Self(Arc::new(Mutex::new(State {
            owner,
            slots: vec![SLOT_BYTES; slots],
            retained,
        }))))
    }
    pub(super) fn admit(&self, index: usize, output: &ToolOutput) -> Result<(), AgentLoopError> {
        // The original, hook input/result, and publication copy may coexist.
        let bytes = output
            .prepared_bytes()
            .and_then(|bytes| bytes.checked_mul(3))
            .and_then(|bytes| bytes.checked_add(SLOT_BYTES))
            .ok_or_else(invalid)?;
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old = *state.slots.get(index).ok_or_else(invalid)?;
        let bytes = bytes.max(old);
        let retained = state
            .retained
            .checked_sub(old)
            .and_then(|n| n.checked_add(bytes))
            .filter(|bytes| *bytes <= OUTPUT_BYTES)
            .ok_or_else(invalid)?;
        state.owner.resize(retained + SCRATCH_BYTES)?;
        state.retained = retained;
        state.slots[index] = bytes;
        Ok(())
    }
    pub(super) fn admit_execution(&self, execution: &mut ToolExecution) {
        if self.admit(execution.call.index, &execution.output).is_err() {
            reject(execution);
        }
    }
    pub(super) fn settled(&self, execution: &ToolExecution) {
        let Some(bytes) = execution
            .output
            .prepared_bytes()
            .and_then(|n| n.checked_mul(3))
            .and_then(|n| n.checked_add(SLOT_BYTES))
        else {
            return;
        };
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old = state.slots[execution.call.index];
        if bytes < old {
            let retained = state.retained - (old - bytes);
            if state.owner.resize(retained + SCRATCH_BYTES).is_ok() {
                state.retained = retained;
                state.slots[execution.call.index] = bytes;
            }
        }
    }
    pub(super) fn finish_profile(&self, retained: usize) -> Result<(), AgentLoopError> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // All callbacks and the profiling encoder have ended; only the IR and owner remain.
        let bytes = retained
            .checked_add(SLOT_BYTES * state.slots.len())
            .ok_or_else(invalid)?;
        state.owner.resize(bytes)?;
        Ok(())
    }
}
pub(super) fn reject(execution: &mut ToolExecution) {
    execution.output = ToolOutput::Text {
        text: REJECTED_OUTPUT.into(),
    };
    execution.presentation = None;
    execution.is_error = true;
}
fn invalid() -> AgentLoopError {
    AgentLoopError::Persistence(REJECTED_OUTPUT.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Meter(Arc<AtomicUsize>);
    impl HistoryWorkingAllowance for Meter {
        fn resize(&mut self, bytes: usize) -> Result<(), AgentLoopError> {
            self.0.store(bytes, Ordering::SeqCst);
            Ok(())
        }
    }
    impl Drop for Meter {
        fn drop(&mut self) {
            self.0.store(0, Ordering::SeqCst);
        }
    }
    #[test]
    fn accumulated_results_reject_before_growth_and_worker_clone_retains_charge() {
        let bytes = Arc::new(AtomicUsize::new(SCRATCH_BYTES + 2 * SLOT_BYTES));
        let budget = ToolResultBudget(Arc::new(Mutex::new(State {
            owner: Box::new(Meter(bytes.clone())),
            slots: vec![SLOT_BYTES; 2],
            retained: 2 * SLOT_BYTES,
        })));
        let output = ToolOutput::Text {
            text: "x".repeat(20 * 1024 * 1024),
        };
        budget.admit(0, &output).expect("first result");
        let first = bytes.load(Ordering::SeqCst);
        assert!(budget.admit(1, &output).is_err());
        assert_eq!(bytes.load(Ordering::SeqCst), first);
        let worker = budget.clone();
        drop(output);
        drop(budget);
        assert_eq!(bytes.load(Ordering::SeqCst), first);
        drop(worker);
        assert_eq!(bytes.load(Ordering::SeqCst), 0);
    }
}
