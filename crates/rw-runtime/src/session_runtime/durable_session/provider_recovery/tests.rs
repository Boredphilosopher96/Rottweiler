#![cfg(test)]
#![allow(clippy::expect_used)]
use super::*;
use crate::journal_service::JournalService;
use rw_core::{commit_session_events, provider_admission::ProviderAdmission};
use rw_store::session::{
    SessionEventLog, UtcTimestamp,
    reservations::{BudgetCharge, BudgetChargeBound, BudgetLedger, BudgetReservationPlan},
};
use rw_types::{
    AccountingAttribution, Cost, EngineEvent, EventMeta, PROTOCOL_VERSION, ProviderCallActuals,
    ProviderCallIdentity, SequenceId, SessionId, TurnId, Usage, config::BudgetConfig,
};
fn plan(call: &str) -> BudgetReservationPlan {
    BudgetReservationPlan {
        identity: ProviderCallIdentity {
            session_id: SessionId("session".into()),
            budget_session_id: SessionId("parent".into()),
            turn_id: TurnId("1".into()),
            attribution: AccountingAttribution::Main,
            call_id: call.into(),
            attempt: 0,
        },
        admitted_at: UtcTimestamp::parse("2026-09-05T12:00:00.000Z").expect("time"),
        input_token_bound: 100,
        output_token_limit: 100,
        charge: BudgetChargeBound::Bounded(BudgetCharge::UsdMicros(10)),
        budget: BudgetConfig::default(),
    }
}
#[tokio::test]
async fn startup_reconciles_only_exact_receipts_and_never_refunds_started_ambiguity() {
    let root = tempfile::tempdir().expect("root");
    let admission = Arc::new(
        DurableProviderAdmission::open(root.path().to_owned())
            .await
            .expect("admission"),
    );
    drop(admission.reserve(plan("reserved")).await.expect("reserve"));
    for call in ["receipted", "uncertain"] {
        drop(
            admission
                .reserve(plan(call))
                .await
                .expect("reserve")
                .start()
                .await
                .expect("start"),
        );
    }
    let journal = SessionEventLog::open(root.path(), "session").expect("journal");
    let sink = DurableEventSink::new(
        journal,
        root.path().to_owned(),
        "session".into(),
        JournalService::new(root.path()).expect("service"),
    )
    .expect("sink");
    sink.configure_canonical(
        Arc::new(rw_ext::ModeRegistry::builtins().expect("modes")),
        None,
    )
    .expect("canonical");
    commit_session_events(
        sink.clone(),
        vec![EngineEvent::ProviderCallAccounted {
            meta: EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: SessionId("session".into()),
                sequence_id: SequenceId(0),
                emitted_at: "2026-09-05T12:00:00.000Z".into(),
                caused_by: None,
            },
            call: plan("receipted").identity,
            actuals: ProviderCallActuals {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: 0,
                },
                cost: Cost::Monetary {
                    amount_micros: 2,
                    currency: "USD".into(),
                },
            },
        }],
    )
    .await
    .expect("durable receipt");
    sink.reconcile_provider_attempts(&admission)
        .await
        .expect("recovery");
    let ledger = BudgetLedger::open(root.path()).expect("authority");
    assert_eq!(
        ledger.phase(&plan("reserved").identity).expect("phase"),
        Some(ProviderCallPhase::Cancelled)
    );
    assert_eq!(
        ledger.phase(&plan("receipted").identity).expect("phase"),
        Some(ProviderCallPhase::Accounted)
    );
    assert_eq!(
        ledger.phase(&plan("uncertain").identity).expect("phase"),
        Some(ProviderCallPhase::Started)
    );
    sink.reconcile_provider_attempts(&admission)
        .await
        .expect("idempotent recovery");
    let pending = admission
        .pending_for_session("session".into(), None, 128)
        .await
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].identity.call_id, "uncertain");
    admission.shutdown().await.expect("shutdown");
}
