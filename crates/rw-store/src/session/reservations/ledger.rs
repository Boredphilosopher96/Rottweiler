//! Transactional provider admission and exact-receipt settlement.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use rw_types::{BudgetScope, Cost, SubscriptionTokenAccounting};
use serde::{Deserialize, Serialize};

use super::{
    BudgetCharge, BudgetChargeBound, BudgetReservationError as Error, BudgetReservationPlan,
    ProviderCallIdentity, ProviderCallReceipt,
    projection::{self, Amounts},
};
use crate::session::{
    SessionStoreError, UtcTimestamp, journal_io::validate_session_id, sqlite_schema,
};

const MAX_ROW_BYTES: usize = 16 * 1024;
/// Maximum unfinished or accounting-ambiguous calls in one accounting root.
pub const MAX_ACTIVE_PROVIDER_CALLS: u128 = 4096;

/// Persistent state of a provider attempt; dropping a caller changes none of these states.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallPhase {
    /// Admitted but provider invocation has not begun.
    Reserved,
    /// Provider invocation may have incurred effects or billing.
    Started,
    /// A durable, usable receipt replaced the reservation.
    Accounted,
    /// A receipt exists but does not establish the actual charge.
    Ambiguous,
    /// Explicitly cancelled before entering the provider.
    Cancelled,
}

/// One unfinished identity returned by bounded startup recovery reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingProviderCall {
    /// Exact host call/attempt to reconcile after acquiring its session writer lease.
    pub identity: ProviderCallIdentity,
    /// Unstarted calls can be released only after proving their previous owner is gone.
    pub phase: ProviderCallPhase,
}

impl ProviderCallPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Started => "started",
            Self::Accounted => "accounted",
            Self::Ambiguous => "ambiguous",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Call {
    identity: ProviderCallIdentity,
    plan: Option<BudgetReservationPlan>,
    phase: ProviderCallPhase,
    receipt: Option<ProviderCallReceipt>,
}

impl Call {
    fn pending(&self) -> Amounts {
        if matches!(
            self.phase,
            ProviderCallPhase::Accounted | ProviderCallPhase::Cancelled
        ) {
            return Amounts::default();
        }
        let bound = self.plan.as_ref().map(|plan| plan.charge);
        let mut amounts = bound
            .and_then(BudgetChargeBound::charge)
            .map_or_else(Amounts::default, Amounts::charge);
        amounts.0[projection::ACTIVE] = 1;
        if !matches!(bound, Some(BudgetChargeBound::Bounded(_))) {
            amounts.0[projection::UNKNOWN] = 1;
        }
        amounts
    }

    fn settled(&self) -> Option<(&str, Amounts)> {
        if self.phase != ProviderCallPhase::Accounted {
            return None;
        }
        let receipt = self.receipt.as_ref()?;
        Some((
            receipt.accounted_at.as_str(),
            Amounts::charge(actual_charge(&receipt.actuals.cost)?),
        ))
    }
}

/// One connection, owned by a bounded storage worker; transactions coordinate processes.
pub struct BudgetLedger {
    connection: Connection,
}

impl BudgetLedger {
    /// Opens the current accounting authority. Turn-only history cannot
    /// establish exact provider receipts and is refused without modification.
    ///
    /// # Errors
    /// Returns storage, schema, or unresolved-history errors without deleting history.
    pub fn open(root: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(root).map_err(SessionStoreError::from)?;
        let connection = Connection::open(root.join("index.sqlite"))?;
        sqlite_schema::validate_accounting(&connection)?;
        super::schema::validate(&connection)?;
        sqlite_schema::configure_connection(&connection)?;
        sqlite_schema::ensure_accounting_schema(&connection)?;
        super::schema::ensure(&connection)?;
        Ok(Self { connection })
    }

    /// Atomically reserves a bounded charge against settled and unfinished calls.
    ///
    /// # Errors
    /// Rejects conflicting identities, exhausted capacity, unknown prior liabilities,
    /// or a cap that cannot admit the plan.
    pub fn reserve(&mut self, plan: &BudgetReservationPlan) -> Result<(), Error> {
        validate_plan(plan)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load(&transaction, &plan.identity)? {
            return if existing.phase == ProviderCallPhase::Reserved
                && existing.plan.as_ref() == Some(plan)
            {
                Ok(())
            } else {
                Err(Error::IdentityConflict)
            };
        }
        let global = projection::read(&transaction, projection::ROOT_SCOPE, 0)?;
        if global.0[projection::ACTIVE] >= MAX_ACTIVE_PROVIDER_CALLS {
            return Err(Error::Capacity);
        }
        let session = projection::read(&transaction, &plan.identity.session_id.0, 0)?;
        admit(&transaction, plan, session, global)?;
        let call = Call {
            identity: plan.identity.clone(),
            plan: Some(plan.clone()),
            phase: ProviderCallPhase::Reserved,
            receipt: None,
        };
        replace_projections(&transaction, None, &call)?;
        save(&transaction, &call)?;
        transaction.commit()?;
        Ok(())
    }

    /// Persists provider entry before the caller invokes any provider code.
    ///
    /// # Errors
    /// A started, cancelled, settled, or unknown identity cannot start again.
    pub fn start(&mut self, identity: &ProviderCallIdentity) -> Result<(), Error> {
        self.transition_unstarted(identity, ProviderCallPhase::Started)
    }

    /// Releases only a reservation whose provider entry has never been recorded.
    ///
    /// # Errors
    /// A started identity remains owned, even if its awaiting caller disappeared.
    pub fn cancel_unstarted(&mut self, identity: &ProviderCallIdentity) -> Result<(), Error> {
        self.transition_unstarted(identity, ProviderCallPhase::Cancelled)
    }

    fn transition_unstarted(
        &mut self,
        identity: &ProviderCallIdentity,
        phase: ProviderCallPhase,
    ) -> Result<(), Error> {
        validate_identity(identity)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = load(&transaction, identity)?.ok_or(Error::IdentityConflict)?;
        if previous.phase != ProviderCallPhase::Reserved {
            return Err(Error::IdentityConflict);
        }
        let mut next = Call {
            identity: previous.identity.clone(),
            plan: previous.plan.clone(),
            phase: previous.phase,
            receipt: previous.receipt.clone(),
        };
        next.phase = phase;
        replace_projections(&transaction, Some(&previous), &next)?;
        save(&transaction, &next)?;
        transaction.commit()?;
        Ok(())
    }

    /// Transfers ownership to an exact durable provider-call accounting event.
    ///
    /// # Errors
    /// Rejects an unstarted/unknown call, mismatched identity, conflicting source
    /// sequence, or invalid receipt. Ambiguous charges retain their admission bound.
    pub fn settle_accounted(&mut self, receipt: &ProviderCallReceipt) -> Result<(), Error> {
        self.record(receipt, false)
    }

    /// Reconciles an authoritative journal receipt, including a missing admission row.
    /// Rewinds never call deletion against this authority.
    ///
    /// # Errors
    /// Returns a receipt validation, identity conflict, or storage error.
    pub fn reconcile_accounted(&mut self, receipt: &ProviderCallReceipt) -> Result<(), Error> {
        self.record(receipt, true)
    }

    fn record(&mut self, receipt: &ProviderCallReceipt, recovery: bool) -> Result<(), Error> {
        validate_receipt(receipt)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = load(&transaction, &receipt.identity)?;
        if !recovery
            && previous.as_ref().is_none_or(|call| {
                matches!(
                    call.phase,
                    ProviderCallPhase::Reserved | ProviderCallPhase::Cancelled
                )
            })
        {
            return Err(Error::IdentityConflict);
        }
        if let Some(old) = previous.as_ref().and_then(|call| call.receipt.as_ref()) {
            if receipt.sequence_id == old.sequence_id {
                return if receipt == old {
                    Ok(())
                } else {
                    Err(Error::IdentityConflict)
                };
            }
            if receipt.sequence_id.0 < old.sequence_id.0 {
                return Ok(());
            }
        }
        let phase = if actual_charge(&receipt.actuals.cost).is_some() {
            ProviderCallPhase::Accounted
        } else {
            ProviderCallPhase::Ambiguous
        };
        let next = Call {
            identity: receipt.identity.clone(),
            plan: previous.as_ref().and_then(|call| call.plan.clone()),
            phase,
            receipt: Some(receipt.clone()),
        };
        replace_projections(&transaction, previous.as_ref(), &next)?;
        save(&transaction, &next)?;
        transaction.commit()?;
        Ok(())
    }

    /// Reads at most 128 unfinished attempts from the session's partial index.
    /// The cursor is the final identity of the prior page. Settled history is not scanned.
    ///
    /// # Errors
    /// Rejects invalid session/cursor identities, page limits, or corrupt stored rows.
    pub fn pending_for_session(
        &self,
        session_id: &str,
        after: Option<&ProviderCallIdentity>,
        limit: u16,
    ) -> Result<Vec<PendingProviderCall>, Error> {
        validate_session_id(session_id)?;
        if limit == 0 || limit > 128 {
            return Err(Error::InvalidPlan(
                "pending provider page limit must be 1 through 128",
            ));
        }
        if let Some(after) = after {
            validate_identity(after)?;
            if after.session_id.0 != session_id {
                return Err(Error::IdentityConflict);
            }
        }
        let mut statement = self.connection.prepare(
            "SELECT call_id,attempt,phase,CASE WHEN length(CAST(data AS BLOB)) <= ?5 THEN data ELSE NULL END FROM provider_calls WHERE session_id=?1 AND phase IN ('reserved','started','ambiguous') AND (call_id,attempt) > (?2,?3) ORDER BY call_id,attempt LIMIT ?4"
        )?;
        let mut rows = statement.query(params![
            session_id,
            after.map_or("", |identity| identity.call_id.as_str()),
            after.map_or(0, |identity| identity.attempt),
            limit,
            i64::try_from(MAX_ROW_BYTES).map_err(|_| Error::Arithmetic)?
        ])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let call_id: String = row.get(0)?;
            let attempt: u32 = row.get(1)?;
            let phase: String = row.get(2)?;
            let data: Option<String> = row.get(3)?;
            let call: Call = serde_json::from_str(
                &data.ok_or(Error::InvalidPlan("oversized provider accounting row"))?,
            )?;
            validate_identity(&call.identity)?;
            if call.identity.session_id.0 != session_id
                || call.identity.call_id != call_id
                || call.identity.attempt != attempt
                || call.phase.as_str() != phase
            {
                return Err(Error::IdentityConflict);
            }
            result.push(PendingProviderCall {
                identity: call.identity,
                phase: call.phase,
            });
        }
        Ok(result)
    }

    /// Reads the retained identity state without materializing other receipt rows.
    ///
    /// # Errors
    /// Returns a malformed identity/row or storage error.
    pub fn phase(
        &self,
        identity: &ProviderCallIdentity,
    ) -> Result<Option<ProviderCallPhase>, Error> {
        validate_identity(identity)?;
        Ok(load(&self.connection, identity)?.map(|call| call.phase))
    }
}

fn replace_projections(
    connection: &Connection,
    previous: Option<&Call>,
    next: &Call,
) -> Result<(), Error> {
    for scope in [projection::ROOT_SCOPE, &next.identity.session_id.0] {
        projection::pending(
            connection,
            scope,
            previous.map_or_else(Amounts::default, Call::pending),
            next.pending(),
        )?;
        if let Some((timestamp, amounts)) = previous.and_then(Call::settled) {
            projection::dated(connection, scope, timestamp, amounts, Amounts::default())?;
        }
        if let Some((timestamp, amounts)) = next.settled() {
            projection::dated(connection, scope, timestamp, Amounts::default(), amounts)?;
        }
    }
    Ok(())
}

fn admit(
    connection: &Connection,
    plan: &BudgetReservationPlan,
    session: Amounts,
    global: Amounts,
) -> Result<(), Error> {
    let Some(charge) = plan.charge.charge() else {
        if [
            plan.budget.session_cost_cap_micros_usd,
            plan.budget.daily_cost_cap_micros_usd,
            plan.budget.session_ai_credit_cap_micros,
            plan.budget.daily_ai_credit_cap_micros,
            plan.budget.session_token_cap,
            plan.budget.daily_token_cap,
        ]
        .contains(&Some(0))
        {
            return Err(Error::UnresolvedCharge);
        }
        return Ok(());
    };
    let (session_cap, daily_cap) = match charge {
        BudgetCharge::UsdMicros(_) => (
            plan.budget.session_cost_cap_micros_usd,
            plan.budget.daily_cost_cap_micros_usd,
        ),
        BudgetCharge::AiCreditMicros(_) => (
            plan.budget.session_ai_credit_cap_micros,
            plan.budget.daily_ai_credit_cap_micros,
        ),
        BudgetCharge::SubscriptionTokens(_) => {
            (plan.budget.session_token_cap, plan.budget.daily_token_cap)
        }
    };
    let at = projection::time_key(plan.admitted_at.as_str());
    let day_start = (at / 1_000_000_000) * 1_000_000_000;
    for (scope, cap, pending) in [
        (BudgetScope::Session, session_cap, session),
        (BudgetScope::Daily, daily_cap, global),
    ] {
        let Some(cap) = cap else {
            continue;
        };
        if matches!(plan.charge, BudgetChargeBound::Bounded(_))
            && pending.0[projection::UNKNOWN] != 0
        {
            return Err(Error::UnresolvedCharge);
        }
        let totals = if scope == BudgetScope::Session {
            projection::through(connection, &plan.identity.session_id.0, at)?
        } else {
            projection::through(connection, projection::ROOT_SCOPE, at)?.subtract(
                projection::through(
                    connection,
                    projection::ROOT_SCOPE,
                    day_start.saturating_sub(1),
                )?,
            )?
        };
        let unit = projection::unit_index(charge);
        let used = totals.0[unit];
        let reserved = pending.0[unit];
        if used
            .checked_add(reserved)
            .and_then(|sum| sum.checked_add(u128::from(charge.amount())))
            .is_none_or(|sum| sum > u128::from(cap))
        {
            return Err(Error::CapExceeded {
                scope,
                requested: charge,
                used: u64::try_from(used).unwrap_or(u64::MAX),
                reserved: u64::try_from(reserved).unwrap_or(u64::MAX),
                cap,
            });
        }
    }
    Ok(())
}

fn actual_charge(cost: &Cost) -> Option<BudgetCharge> {
    match cost {
        Cost::Monetary {
            amount_micros,
            currency,
        } if currency == "USD" => Some(BudgetCharge::UsdMicros(*amount_micros)),
        Cost::AiCredits { credits_micros, .. } => {
            Some(BudgetCharge::AiCreditMicros(*credits_micros))
        }
        Cost::SubscriptionQuota { .. } => match cost.subscription_token_accounting() {
            SubscriptionTokenAccounting::Metered(tokens) => {
                Some(BudgetCharge::SubscriptionTokens(tokens))
            }
            _ => None,
        },
        _ => None,
    }
}

fn load(connection: &Connection, identity: &ProviderCallIdentity) -> Result<Option<Call>, Error> {
    let json: Option<Option<String>> = connection.query_row(
        "SELECT CASE WHEN length(CAST(data AS BLOB)) <= ?4 THEN data ELSE NULL END FROM provider_calls WHERE session_id=?1 AND call_id=?2 AND attempt=?3",
        params![identity.session_id.0, identity.call_id, identity.attempt, i64::try_from(MAX_ROW_BYTES).map_err(|_| Error::Arithmetic)?], |row| row.get(0),
    ).optional()?;
    let Some(json) = json else {
        return Ok(None);
    };
    let call: Call = serde_json::from_str(
        &json.ok_or(Error::InvalidPlan("oversized provider accounting row"))?,
    )?;
    if &call.identity != identity {
        return Err(Error::IdentityConflict);
    }
    Ok(Some(call))
}

fn save(connection: &Connection, call: &Call) -> Result<(), Error> {
    let data = serde_json::to_string(call)?;
    if data.len() > MAX_ROW_BYTES {
        return Err(Error::InvalidPlan(
            "provider accounting row exceeds byte limit",
        ));
    }
    connection.execute(
        "INSERT INTO provider_calls(session_id,call_id,attempt,phase,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session_id,call_id,attempt) DO UPDATE SET phase=excluded.phase,data=excluded.data",
        params![call.identity.session_id.0, call.identity.call_id, call.identity.attempt, call.phase.as_str(), data],
    )?;
    Ok(())
}

fn validate_identity(identity: &ProviderCallIdentity) -> Result<(), Error> {
    validate_session_id(&identity.session_id.0)?;
    for value in [&identity.call_id, &identity.turn_id.0] {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Error::InvalidPlan("invalid host call or turn identity"));
        }
    }
    Ok(())
}

pub(super) fn validate_plan(plan: &BudgetReservationPlan) -> Result<(), Error> {
    validate_identity(&plan.identity)?;
    UtcTimestamp::parse(plan.admitted_at.as_str())?;
    plan.budget.validate().map_err(Error::InvalidPlan)?;
    if plan.output_token_limit == 0 {
        return Err(Error::InvalidPlan("provider output limit must be nonzero"));
    }
    Ok(())
}

pub(super) fn validate_receipt(receipt: &ProviderCallReceipt) -> Result<(), Error> {
    validate_identity(&receipt.identity)?;
    UtcTimestamp::parse(receipt.accounted_at.as_str())?;
    let oversized = match &receipt.actuals.cost {
        Cost::Monetary { currency, .. } => currency.len() > 16,
        Cost::AiCredits {
            nominal_amount_micros,
            currency,
            ..
        } => {
            nominal_amount_micros
                .as_ref()
                .is_some_and(|value| value.len() > 128)
                || currency.as_ref().is_some_and(|value| value.len() > 16)
        }
        Cost::SubscriptionQuota { used, unit } => {
            used.as_ref().is_some_and(|value| value.len() > 128)
                || unit.as_ref().is_some_and(|value| value.len() > 32)
        }
        Cost::Unavailable { reason } => reason.len() > 4096,
    };
    if oversized {
        return Err(Error::InvalidPlan("provider receipt exceeds byte limit"));
    }
    Ok(())
}
