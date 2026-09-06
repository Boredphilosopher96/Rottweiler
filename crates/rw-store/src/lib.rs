//! Configuration and persistent storage for Rottweiler.

pub mod catalog_cache;
pub mod checkpoint;
pub mod command_receipts;
pub mod config;
pub mod credentials;
pub mod memory;
pub mod prompt_shapes;
pub mod session;
pub mod trust;
pub mod workflow;

pub use memory::{MemoryEntry, MemoryError, ProjectMemoryStore};
pub use session::{
    AccountingLedger, AccountingTotals, TurnAccountingEntry, UtcDayKey, UtcTimestamp,
};
