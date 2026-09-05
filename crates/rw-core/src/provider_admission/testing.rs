//! Test-owned durable admission; scripted models may explicitly ignore invocations.
#![allow(clippy::expect_used)]
use super::*;
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
