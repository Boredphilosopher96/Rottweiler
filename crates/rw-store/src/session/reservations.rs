//! Provider-call budget identities and admission bounds shared by durable storage and core.

use rw_types::{
    AccountingAttribution, BudgetScope, BudgetUnit, Cost, SessionId, TurnId, Usage,
    config::BudgetConfig,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{SessionStoreError, UtcTimestamp};

/// Host-assigned logical call identity plus a distinct provider attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCallIdentity {
    /// Session which owns the provider request.
    pub session_id: SessionId,
    /// Durable agent turn which owns this request.
    pub turn_id: TurnId,
    /// Separates ordinary generation from compaction, title, and child usage.
    pub attribution: AccountingAttribution,
    /// Bounded host-generated identity, never a model-supplied tool identifier.
    pub call_id: String,
    /// Retries under a logical call must increment this value.
    pub attempt: u32,
}

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

/// Provider-reported actuals; a missing or ambiguous terminal is never represented as zero.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCallActuals {
    /// Normalized input, output, cache and reasoning usage.
    pub usage: Usage,
    /// Normalized monetary, credit, subscription, or unavailable accounting.
    pub cost: Cost,
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
