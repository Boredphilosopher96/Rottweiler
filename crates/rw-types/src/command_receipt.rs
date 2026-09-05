//! Correlated mutation outcomes whose identity survives client reconnection.
use crate::{CommandOutcome, EngineEvent};
use rw_memory_derive::PrepareAllocation as Allocation;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, Allocation)]
#[serde(deny_unknown_fields)]
pub struct CommandReceipt {
    pub outcome: CommandOutcome,
    pub events: Vec<EngineEvent>,
}

#[derive(Clone, Debug)]
pub enum ReceiptAdmission {
    Admitted,
    Indeterminate,
    Completed(CommandReceipt),
}
