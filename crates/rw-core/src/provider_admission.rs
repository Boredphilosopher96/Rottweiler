//! Owned provider-call admission. Dropping a permit never proves that billing stopped.

use async_trait::async_trait;
pub use rw_store::session::reservations::{
    BudgetCharge, BudgetChargeBound, BudgetReservationError, BudgetReservationPlan,
    ProviderCallActuals, ProviderCallIdentity, ProviderCallReceipt,
};

/// Application-scoped admission service backed by the shared accounting root.
#[async_trait]
pub trait ProviderAdmission: Send + Sync {
    /// Atomically charges the plan against durable usage and other reservations.
    /// Implementations retain storage-job ownership if the awaiting caller disappears.
    async fn reserve(
        &self,
        plan: BudgetReservationPlan,
    ) -> Result<Box<dyn ReservedProviderCall>, BudgetReservationError>;
}

/// An admitted request which has not yet entered the provider.
#[async_trait]
pub trait ReservedProviderCall: Send {
    /// Persists the started state before any provider-side work is invoked.
    async fn start(self: Box<Self>) -> Result<Box<dyn ActiveProviderCall>, BudgetReservationError>;

    /// Releases only a request proven not to have entered the provider.
    async fn cancel_unstarted(self: Box<Self>) -> Result<(), BudgetReservationError>;
}

/// A provider attempt whose effects and accounting remain owned until reconciliation.
#[async_trait]
pub trait ActiveProviderCall: Send {
    /// Transfers the reservation to the exact durable provider-call accounting fact.
    /// The caller must first append `EngineEvent::ProviderCallAccounted` and await
    /// durable completion. Replaying the same receipt is idempotent; a later
    /// source sequence may correct actual usage. Ambiguous actuals retain their
    /// admission charge. A turn summary cannot be supplied as a call receipt.
    async fn settle_accounted(
        &mut self,
        receipt: ProviderCallReceipt,
    ) -> Result<(), BudgetReservationError>;
}
