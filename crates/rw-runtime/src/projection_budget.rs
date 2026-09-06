//! One catch-up allowance shared by all indexed sources used by a direct read.

const MAX_READ_PROJECTION_BATCHES: usize = 4;

pub(crate) struct ProjectionBudget {
    remaining: usize,
}
impl ProjectionBudget {
    pub(crate) const fn new() -> Self {
        Self {
            remaining: MAX_READ_PROJECTION_BATCHES,
        }
    }
    pub(crate) fn take_batch(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}
