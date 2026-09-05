//! Semantic history capabilities with one immutable source/index snapshot per reader.
use super::{ConversationCut, ConversationPage, HistoryMaterializationLimits, RecoveryBootstrap};
use crate::engine::AgentLoopError;
use async_trait::async_trait;
use rw_types::SequenceId;
use std::{ops::Range, sync::Arc};

/// An application-owned canonical history service for one exact session.
#[async_trait]
pub trait SessionHistory: Send + Sync {
    /// Capture one committed history generation. The returned owner retains its
    /// read admission and storage descriptors until the last reference is dropped.
    async fn capture_history(&self) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError>;
}

/// Canonical context, independent of raw audit replay and display transcript rows.
/// Appends, receipts and rewinds cannot change a captured view's source identities.
#[async_trait]
pub trait SessionHistoryView: Send + Sync {
    fn through(&self) -> Option<SequenceId>;
    fn conversation(&self) -> ConversationCut;

    /// Resolve live controls and interrupted input at this exact prefix.
    async fn bootstrap(&self) -> Result<RecoveryBootstrap, AgentLoopError>;

    /// Read a bounded contiguous context interval. The implementation may cut the
    /// requested interval at admission, and must return its exact resume ordinal.
    async fn conversation_page(
        &self,
        range: Range<u64>,
        limits: HistoryMaterializationLimits,
    ) -> Result<ConversationPage, AgentLoopError>;
}
