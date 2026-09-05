//! Provider-call budget identities and admission bounds shared by durable storage and core.

mod ledger;
mod projection;
mod schema;
#[cfg(test)]
mod tests;

pub use ledger::{BudgetLedger, MAX_ACTIVE_PROVIDER_CALLS, ProviderCallPhase};

use rw_types::{BudgetScope, BudgetUnit, SequenceId, config::BudgetConfig};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{SessionStoreError, UtcTimestamp};
pub use rw_types::{ProviderCallActuals, ProviderCallIdentity};

/// A charge in exactly one supported billing unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "unit", content = "amount", rename_all = "snake_case")]
pub enum BudgetCharge {
    /// Ordinary API cost in micro-US-dollars.
    UsdMicros(u64),
    /// Provider credits in micro-credit units.
    AiCreditMicros(u64),
    /// Metered subscription quota in tokens.
    SubscriptionTokens(u64),
}

/// Whether the request's admitted charge is a bound or only an estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "assurance", content = "charge", rename_all = "snake_case")]
pub enum BudgetChargeBound {
    /// Pricing and enforced input/output limits provide a request upper bound.
    Bounded(BudgetCharge),
    /// Unknown pricing or provider accounting cannot support a strict spend promise.
    BestEffort(Option<BudgetCharge>),
}

/// Complete bounded metadata submitted to the accounting-root admission transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BudgetReservationPlan {
    /// Stable request identity used for idempotency and recovery.
    pub identity: ProviderCallIdentity,
    /// Injected event clock, also defining the UTC-day budget scope.
    pub admitted_at: UtcTimestamp,
    /// Bound from the final materialized request, including tools and cached input.
    pub input_token_bound: u64,
    /// Output limit actually sent to the provider for this attempt.
    pub output_token_limit: u64,
    /// Charge assurance derived from this provider route and these token limits.
    pub charge: BudgetChargeBound,
    /// Effective session and root-wide daily guardrails.
    pub budget: BudgetConfig,
}

/// Exact journal event whose charge can replace one provider attempt's reservation.
/// Callers submit this only after the event append has completed durably.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCallReceipt {
    /// Identity from the durable provider accounting event.
    pub identity: ProviderCallIdentity,
    /// Exact source sequence; later corrections supersede earlier receipts.
    pub sequence_id: SequenceId,
    /// Injected event time, which assigns settled usage to its UTC day.
    pub accounted_at: UtcTimestamp,
    /// Provider-normalized actuals from that exact event.
    pub actuals: ProviderCallActuals,
}

/// Admission can fail without starting provider work.
#[derive(Debug, Error)]
pub enum BudgetReservationError {
    /// Request metadata is invalid or exceeds a fixed storage bound.
    #[error("invalid provider budget reservation: {0}")]
    InvalidPlan(&'static str),
    /// A call identity already owns a different plan or has already started.
    #[error("provider call identity is already owned")]
    IdentityConflict,
    /// The remaining cap cannot admit the requested bound.
    #[error(
        "{scope:?} budget cannot admit {requested:?}; used={used}, reserved={reserved}, cap={cap}"
    )]
    CapExceeded {
        /// Session or UTC-day scope.
        scope: BudgetScope,
        /// Requested charge and its billing unit.
        requested: BudgetCharge,
        /// Durable, already-accounted charge in this unit.
        used: u64,
        /// Concurrent and unresolved request charge in this unit.
        reserved: u64,
        /// Configured maximum in this unit.
        cap: u64,
    },
    /// The fixed admission queue or retained reservation capacity is exhausted.
    #[error("provider budget admission capacity is exhausted")]
    Capacity,
    /// Durable accounting could not establish the requested transition.
    #[error(transparent)]
    Store(#[from] SessionStoreError),
    /// The owned storage task did not complete normally.
    #[error("provider budget storage task failed: {0}")]
    Worker(String),
    /// A strict cap cannot include unknown outstanding provider liabilities.
    #[error("strict budget admission requires reconciliation of unknown charges")]
    UnresolvedCharge,
    /// Checked projection arithmetic refused an overflow or inconsistent subtraction.
    #[error("provider accounting projection arithmetic is inconsistent or exhausted")]
    Arithmetic,
    /// Durable `SQLite` authority could not complete the transaction.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// A bounded provider accounting record is malformed.
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
}

impl BudgetCharge {
    /// Returns the canonical protocol billing unit for diagnostics.
    #[must_use]
    pub fn unit(self) -> BudgetUnit {
        match self {
            Self::UsdMicros(_) => BudgetUnit::MicrosUsd,
            Self::AiCreditMicros(_) => BudgetUnit::AiCreditMicros,
            Self::SubscriptionTokens(_) => BudgetUnit::Tokens,
        }
    }

    /// Returns the charge without losing its unit at the admission boundary.
    #[must_use]
    pub fn amount(self) -> u64 {
        match self {
            Self::UsdMicros(value)
            | Self::AiCreditMicros(value)
            | Self::SubscriptionTokens(value) => value,
        }
    }
}

impl BudgetChargeBound {
    /// Returns a reservable charge, including an explicitly best-effort estimate.
    #[must_use]
    pub fn charge(self) -> Option<BudgetCharge> {
        match self {
            Self::Bounded(charge) => Some(charge),
            Self::BestEffort(charge) => charge,
        }
    }
}

impl BudgetReservationPlan {
    /// Validates fixed admission metadata before it enters a storage queue.
    ///
    /// # Errors
    /// Rejects invalid identity, time, configuration, or output limits.
    pub fn validate(&self) -> Result<(), BudgetReservationError> {
        ledger::validate_plan(self)
    }
}

impl ProviderCallReceipt {
    /// Validates the receipt's identity, timestamp and bounded accounting strings.
    ///
    /// # Errors
    /// Rejects malformed or oversized external accounting metadata.
    pub fn validate(&self) -> Result<(), BudgetReservationError> {
        ledger::validate_receipt(self)
    }
}
