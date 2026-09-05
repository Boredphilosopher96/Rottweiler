//! Semantic history capabilities with one immutable source/index snapshot per reader.
use super::{ConversationCut, ConversationPage, HistoryMaterializationLimits, RecoveryBootstrap};
use crate::engine::AgentLoopError;
use async_trait::async_trait;
use rw_types::SequenceId;
use std::{
    ops::{Deref, Range},
    sync::Arc,
};

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
    async fn bootstrap(&self) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError>;

    /// Resolve a completed rewind boundary without materializing its conversation.
    async fn recovery_at_completed_turn(
        &self,
        turn: u64,
    ) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError>;

    /// Read a bounded contiguous context interval. The implementation may cut the
    /// requested interval at admission, and must return its exact resume ordinal.
    async fn conversation_page(
        &self,
        range: Range<u64>,
        limits: HistoryMaterializationLimits,
    ) -> Result<HistoryRead<ConversationPage>, AgentLoopError>;
}

/// An admitted materialization. Its resource owner survives delivery, independent
/// of the captured view. Borrowing or mapping the result cannot release its charge.
pub struct HistoryRead<T> {
    value: T,
    owner: Box<dyn Send + Sync>,
}
impl<T> HistoryRead<T> {
    /// Transfer the materialization and its already-acquired allowance together.
    #[must_use]
    pub fn new(value: T, owner: impl Send + Sync + 'static) -> Self {
        Self {
            value,
            owner: Box::new(owner),
        }
    }

    /// Transform an admitted result without releasing the original allowance.
    /// The transformation must fit that allowance; this does not grant new memory.
    #[must_use]
    pub fn map<U>(self, transform: impl FnOnce(T) -> U) -> HistoryRead<U> {
        HistoryRead {
            value: transform(self.value),
            owner: self.owner,
        }
    }
}
impl<T> Deref for HistoryRead<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
