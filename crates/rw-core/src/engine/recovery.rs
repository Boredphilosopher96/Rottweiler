//! Canonical recovery and exact provider history; independent of display transcript rows.

mod active;
pub use active::InterruptedTurnInputs;
mod accounting;
pub use accounting::{MAX_ACCOUNTING_PAGE_BYTES, RecoveryAccountingPage};
mod control;
pub use control::{
    MAX_CONTROL_SOURCE_BYTES, RecoveredMessage, RecoveredModelSelection, RecoveryControlPayloads,
};
mod encoding;
mod maintenance;
mod projector;
mod read;
mod reduce;
pub use read::{
    CanonicalHistory, HistoryMaterializationLimits, MAX_MATERIALIZED_HISTORY_BYTES,
    MAX_MATERIALIZED_HISTORY_TURNS, RecoverySnapshot,
};
mod state;
mod window;
mod workspace;
pub use projector::{CanonicalRecovery, RecoveryProgress};
use rw_store::session::recovery_index::RecoveryIndexError;
pub use state::{
    AcceptedSource, ActiveTurn, ConversationCut, ConversationSource, QuestionSource, QueuedSource,
    RecoveryControl, RecoveryHead, SourceTotals, ToolStartIdentity, TurnSourceKind,
};
use thiserror::Error;
pub use window::RecoveryBoundary;
pub use workspace::WorkspaceBootstrap;

/// A canonical recovery operation cannot safely continue.
#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("invalid canonical recovery state: {0}")]
    Invalid(&'static str),
    #[error("canonical recovery capacity exceeded: {0}")]
    Limit(&'static str),
    #[error("canonical recovery registry changed; rebuild required")]
    RegistryChanged,
    #[error("canonical recovery maintenance is not yet published")]
    Maintenance,
    #[error(transparent)]
    Store(#[from] RecoveryIndexError),
    #[error(transparent)]
    Journal(#[from] rw_store::session::SessionStoreError),
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
    #[error(transparent)]
    Projection(#[from] super::SessionProjectionError),
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod active_tests;

#[cfg(test)]
mod control_tests;

#[cfg(test)]
mod indexed_read_tests;

#[cfg(test)]
mod workspace_tests;
