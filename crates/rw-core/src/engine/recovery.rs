//! Canonical recovery and exact provider history; independent of display transcript rows.

mod active;
mod allocation;
pub use active::InterruptedTurnInputs;
pub use allocation::MAX_HISTORY_RESULT_BYTES;
mod accounting;
pub use accounting::{
    AccountingReconciliationPage, MAX_ACCOUNTING_PAGE_BYTES, RecoveryAccountingPage,
};
mod capability;
pub use capability::{HistoryRead, HistoryWorkingAllowance, SessionHistory, SessionHistoryView};
mod context_selection;
mod context_state;
mod control;
mod prompts;
mod pruning;
pub use control::{
    MAX_CONTROL_SOURCE_BYTES, RecoveredMessage, RecoveredModelSelection, RecoveryBootstrap,
    RecoveryControlPayloads,
};
mod encoding;
mod fragments;
pub use fragments::{
    ConversationFragment, ConversationFragmentCursor, ConversationFragmentSource,
    MAX_SUMMARY_FRAGMENT_BYTES,
};
mod extension;
pub(in crate::engine) mod input;
mod maintenance;
mod projector;
mod read;
pub use input::materialize_conversation_event;
mod receipts;
mod routing;
pub use routing::SessionRoutingIndex;
mod reduce;
mod repair;
pub use read::{
    CanonicalHistory, HistoryMaterializationLimits, MAX_MATERIALIZED_HISTORY_BYTES,
    MAX_MATERIALIZED_HISTORY_DECODE_BYTES, MAX_MATERIALIZED_HISTORY_TURNS, RecoverySnapshot,
};
pub use repair::InterruptedTurnRecovery;
mod pages;
mod state;
mod subagents;
pub use subagents::{SubagentBinding, SubagentLifecycleIndex, SubagentLifecycleView};
mod window;
pub use pages::ConversationPage;
mod workspace;
pub use projector::{CanonicalRecovery, RecoveryProgress};
use rw_store::session::recovery_index::RecoveryIndexError;
pub use state::{
    AcceptedSource, ActiveTurn, ConversationCut, ConversationMetadata, ConversationSource,
    QuestionSource, QueuedSource, RecoveryControl, RecoveryHead, SourceTotals, ToolStartIdentity,
    TurnSourceKind,
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

#[cfg(test)]
mod source_lookup_tests;

#[cfg(test)]
mod accounting_tests;

#[cfg(test)]
mod page_tests;

#[cfg(test)]
mod model_tests;

#[cfg(test)]
mod context_state_tests;

#[cfg(test)]
mod citation_tests;

#[cfg(test)]
mod repair_tests;

#[cfg(test)]
mod input_tests;

#[cfg(test)]
mod prompt_tests;

#[cfg(test)]
mod input_commit_tests;

#[cfg(test)]
mod context_selection_tests;

#[cfg(test)]
mod test_source;

#[cfg(test)]
mod fragment_input_tests;
