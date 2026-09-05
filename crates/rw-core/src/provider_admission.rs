//! Owned provider-call admission. Dropping a permit never proves that billing stopped.

pub(crate) mod gate;

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

/// Whether final request admission has a proven input bound or only an estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderInputBudget {
    /// Enforced input bound, including tools, framing, cached input and attachments.
    Bounded(u64),
    /// Estimation supports best-effort accounting only.
    Estimated(u64),
}

/// Session-owned logical call context; retries receive distinct attempt identities.
/// Runtime callbacks never enter provider wire requests or recordings.
#[derive(Clone)]
pub struct ProviderInvocation {
    /// Durable session which owns this logical call.
    pub session_id: rw_types::SessionId,
    /// Durable parent turn, including title and compaction attribution.
    pub turn_id: rw_types::TurnId,
    /// Accounting role of this call.
    pub attribution: rw_types::AccountingAttribution,
    /// Bounded host-generated logical call identity.
    pub call_id: String,
    /// Input bound from the final request admission step.
    pub input: ProviderInputBudget,
    /// Effective guardrails shared across every candidate and retry.
    pub budget: rw_types::config::BudgetConfig,
    /// Injected clock sampled before each actual attempt.
    pub clock: std::sync::Arc<dyn crate::EventClock>,
    /// Application-owned durable budget admission service.
    pub admission: std::sync::Arc<dyn ProviderAdmission>,
    /// Session-owned writer returning exact durable accounting receipts.
    pub accounting: std::sync::Arc<dyn ProviderAccountingSink>,
}

/// Appends exact provider accounting without exposing storage internals to routing.
#[async_trait]
pub trait ProviderAccountingSink: Send + Sync {
    /// Completes only after the supplied call actuals have been durably appended.
    async fn append_accounted(
        &self,
        identity: ProviderCallIdentity,
        actuals: ProviderCallActuals,
    ) -> Result<ProviderCallReceipt, BudgetReservationError>;
}
