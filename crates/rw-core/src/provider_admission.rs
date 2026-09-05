//! Owned provider-call admission. Dropping a permit never proves that billing stopped.

use async_trait::async_trait;
pub use rw_store::session::reservations::{
    BudgetCharge, BudgetChargeBound, BudgetReservationError, BudgetReservationPlan,
    ProviderCallActuals, ProviderCallIdentity,
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
    /// Records authoritative terminal actuals after provider effect settlement.
    /// This replaces the planned charge; it does not release the reservation.
    /// The matching durable turn accounting transaction transfers the charge,
    /// scoped by session, turn and attribution. Ambiguous actuals remain reserved.
    async fn record_terminal(
        &mut self,
        actuals: ProviderCallActuals,
    ) -> Result<(), BudgetReservationError>;
}
