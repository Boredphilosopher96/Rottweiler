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

    /// Reserve the application-wide working allowance before context transformation.
    /// The owner must remain live through temporary buffers and delivered results.
    fn reserve_working_set(&self) -> Result<HistoryRead<()>, AgentLoopError>;

    /// Resolve live controls and interrupted input at this exact prefix.
    async fn bootstrap(&self) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError>;

    /// Resolve a completed rewind boundary without materializing its conversation.
    async fn recovery_at_completed_turn(
        &self,
        turn: u64,
    ) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError>;

    /// Select one recorded prompt source cut without replaying the journal.
    async fn prompt_at_turn(
        &self,
        turn: u64,
    ) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError>;

    /// Prove that a reconstructed historical prompt matches its recorded request.
    /// Called inside the retained blocking context worker.
    fn verify_prompt(&self, turn: u64, dump: &rw_types::PromptDump) -> Result<(), AgentLoopError>;

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

    /// Transfer a decoded value and its allowance separately to an admitted encoder.
    /// The returned guard must outlive the value until another owner takes its charge.
    #[must_use]
    pub fn into_parts(self) -> (T, HistoryRead<()>) {
        (
            self.value,
            HistoryRead {
                value: (),
                owner: self.owner,
            },
        )
    }

    /// Attach another existing allowance without releasing either resource owner.
    #[must_use]
    pub fn retain(self, owner: impl Send + Sync + 'static) -> Self {
        Self {
            value: self.value,
            owner: Box::new((self.owner, owner)),
        }
    }

    /// Transform an admitted result, releasing its allowance only if transformation fails.
    /// # Errors
    /// Returns the transformation error.
    pub fn try_map<U, E>(
        self,
        transform: impl FnOnce(T) -> Result<U, E>,
    ) -> Result<HistoryRead<U>, E> {
        Ok(HistoryRead {
            value: transform(self.value)?,
            owner: self.owner,
        })
    }

    /// Keep the source allowance through an asynchronous transformation and its result.
    /// The transformation must fit the admitted allocation just like `map`.
    pub async fn map_async<U, F: std::future::Future<Output = U>>(
        self,
        transform: impl FnOnce(T) -> F,
    ) -> HistoryRead<U> {
        let value = transform(self.value).await;
        HistoryRead {
            value,
            owner: self.owner,
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
impl<T> HistoryRead<HistoryRead<T>> {
    /// Combine nested materializations while retaining both existing allowances.
    #[must_use]
    pub fn flatten(self) -> HistoryRead<T> {
        HistoryRead {
            value: self.value.value,
            owner: Box::new((self.owner, self.value.owner)),
        }
    }
}
impl<T> Deref for HistoryRead<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
