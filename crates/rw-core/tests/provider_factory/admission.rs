//! Test-owned durable admission; scripted models may explicitly ignore invocations.
#![allow(clippy::expect_used)]
use async_trait::async_trait;
use rw_core::provider_admission::*;
use rw_store::session::reservations::BudgetLedger;
use std::sync::{Arc, Mutex};

pub(crate) fn admission() -> Arc<dyn ProviderAdmission> {
    let root = tempfile::tempdir().expect("test accounting root");
    Arc::new(TestAdmission {
        ledger: Arc::new(Mutex::new(
            BudgetLedger::open(root.path()).expect("test ledger"),
        )),
        _root: root,
    })
}
struct TestAdmission {
    ledger: Arc<Mutex<BudgetLedger>>,
    _root: tempfile::TempDir,
}
struct TestReserved {
    ledger: Arc<Mutex<BudgetLedger>>,
    identity: ProviderCallIdentity,
}
#[async_trait]
impl ProviderAdmission for TestAdmission {
    async fn reserve(
        &self,
        plan: BudgetReservationPlan,
    ) -> Result<Box<dyn ReservedProviderCall>, BudgetReservationError> {
        self.ledger.lock().expect("ledger").reserve(&plan)?;
        Ok(Box::new(TestReserved {
            ledger: Arc::clone(&self.ledger),
            identity: plan.identity,
        }))
    }
}
#[async_trait]
impl ReservedProviderCall for TestReserved {
    async fn start(self: Box<Self>) -> Result<Box<dyn ActiveProviderCall>, BudgetReservationError> {
        self.ledger.lock().expect("ledger").start(&self.identity)?;
        Ok(self)
    }
    async fn cancel_unstarted(self: Box<Self>) -> Result<(), BudgetReservationError> {
        self.ledger
            .lock()
            .expect("ledger")
            .cancel_unstarted(&self.identity)
    }
}
#[async_trait]
impl ActiveProviderCall for TestReserved {
    async fn settle_accounted(
        &mut self,
        receipt: ProviderCallReceipt,
    ) -> Result<(), BudgetReservationError> {
        self.ledger
            .lock()
            .expect("ledger")
            .settle_accounted(&receipt)
    }
}

struct FixtureAccounting;
#[async_trait]
impl ProviderAccountingSink for FixtureAccounting {
    async fn append_accounted(
        &self,
        identity: ProviderCallIdentity,
        actuals: ProviderCallActuals,
    ) -> Result<ProviderCallReceipt, BudgetReservationError> {
        Ok(ProviderCallReceipt {
            identity,
            actuals,
            sequence_id: rw_types::SequenceId(0),
            accounted_at: rw_store::session::UtcTimestamp::from_unix_millis(0)?,
        })
    }
}
pub(super) fn invocation() -> ProviderInvocation {
    ProviderInvocation {
        session_id: rw_types::SessionId("fixture".into()),
        turn_id: rw_types::TurnId("1".into()),
        attribution: rw_types::AccountingAttribution::Main,
        call_id: "fixture-call".into(),
        input: ProviderInputBudget::Estimated(128),
        budget: rw_types::config::BudgetConfig::default(),
        clock: Arc::new(rw_core::SystemEventClock),
        admission: admission(),
        accounting: Arc::new(FixtureAccounting),
    }
}
