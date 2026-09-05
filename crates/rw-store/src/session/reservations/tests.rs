#![allow(clippy::unwrap_used)]

use super::*;
use rw_types::{AccountingAttribution, Cost, SessionId, TurnId, Usage};
use std::sync::{Arc, Barrier};

fn plan(session: &str, call: &str, amount: u64) -> BudgetReservationPlan {
    BudgetReservationPlan {
        identity: ProviderCallIdentity {
            budget_session_id: SessionId(session.into()),
            session_id: SessionId(session.into()),
            turn_id: TurnId("turn-1".into()),
            attribution: AccountingAttribution::Main,
            call_id: call.into(),
            attempt: 0,
        },
        admitted_at: UtcTimestamp::parse("2026-09-04T12:00:00.000Z").unwrap(),
        input_token_bound: 100,
        output_token_limit: 100,
        charge: BudgetChargeBound::Bounded(BudgetCharge::UsdMicros(amount)),
        budget: BudgetConfig {
            session_cost_cap_micros_usd: Some(100),
            daily_cost_cap_micros_usd: Some(100),
            ..BudgetConfig::default()
        },
    }
}
fn receipt(plan: &BudgetReservationPlan, sequence: u64, amount: u64) -> ProviderCallReceipt {
    ProviderCallReceipt {
        identity: plan.identity.clone(),
        sequence_id: sequence.into(),
        accounted_at: plan.admitted_at.clone(),
        actuals: ProviderCallActuals {
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Monetary {
                amount_micros: amount,
                currency: "USD".into(),
            },
        },
    }
}

#[test]
fn concurrent_connections_cannot_overspend_the_daily_cap() {
    let root = tempfile::tempdir().unwrap();
    BudgetLedger::open(root.path()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|index| {
            let root = root.path().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut ledger = BudgetLedger::open(&root).unwrap();
                barrier.wait();
                ledger.reserve(&plan(&format!("session-{index}"), "call", 60))
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(BudgetReservationError::CapExceeded {
                    scope: BudgetScope::Daily,
                    ..
                })
            ))
            .count(),
        1
    );
}

#[test]
fn started_work_survives_owner_drop_and_cannot_be_refunded_or_started_twice() {
    let root = tempfile::tempdir().unwrap();
    let request = plan("session", "call", 80);
    {
        let mut ledger = BudgetLedger::open(root.path()).unwrap();
        ledger.reserve(&request).unwrap();
        ledger.start(&request.identity).unwrap();
    }
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    assert!(matches!(
        ledger.cancel_unstarted(&request.identity),
        Err(BudgetReservationError::IdentityConflict)
    ));
    assert!(matches!(
        ledger.start(&request.identity),
        Err(BudgetReservationError::IdentityConflict)
    ));
    assert!(matches!(
        ledger.reserve(&plan("session", "next", 21)),
        Err(BudgetReservationError::CapExceeded { .. })
    ));
    ledger.settle_accounted(&receipt(&request, 5, 30)).unwrap();
    ledger.reserve(&plan("session", "next", 70)).unwrap();
}

#[test]
fn exact_receipts_are_idempotent_and_later_corrections_replace_actuals() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let request = plan("session", "call", 80);
    ledger.reserve(&request).unwrap();
    ledger.start(&request.identity).unwrap();
    ledger.settle_accounted(&receipt(&request, 5, 30)).unwrap();
    ledger.settle_accounted(&receipt(&request, 5, 30)).unwrap();
    assert!(matches!(
        ledger.settle_accounted(&receipt(&request, 5, 20)),
        Err(BudgetReservationError::IdentityConflict)
    ));
    ledger.settle_accounted(&receipt(&request, 6, 40)).unwrap();
    ledger.settle_accounted(&receipt(&request, 5, 30)).unwrap();
    assert!(matches!(
        ledger.reserve(&plan("session", "next", 61)),
        Err(BudgetReservationError::CapExceeded { used: 40, .. })
    ));
    ledger.reserve(&plan("session", "next", 60)).unwrap();
}

#[test]
fn unavailable_actuals_retain_the_bound_across_midnight() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let request = plan("session", "call", 80);
    ledger.reserve(&request).unwrap();
    ledger.start(&request.identity).unwrap();
    let mut unknown = receipt(&request, 5, 0);
    unknown.actuals.cost = Cost::Unavailable {
        reason: "terminal lost".into(),
    };
    ledger.settle_accounted(&unknown).unwrap();
    assert_eq!(
        ledger.phase(&request.identity).unwrap(),
        Some(ProviderCallPhase::Ambiguous)
    );
    let mut next = plan("other-session", "next", 21);
    next.admitted_at = UtcTimestamp::parse("2026-09-05T00:00:00.000Z").unwrap();
    assert!(matches!(
        ledger.reserve(&next),
        Err(BudgetReservationError::CapExceeded { reserved: 80, .. })
    ));
}

#[test]
fn bounded_admission_refuses_unknown_liabilities_and_cancel_releases_only_unstarted() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let mut unknown = plan("session", "unknown", 0);
    unknown.charge = BudgetChargeBound::BestEffort(None);
    ledger.reserve(&unknown).unwrap();
    assert!(matches!(
        ledger.reserve(&plan("session", "next", 1)),
        Err(BudgetReservationError::UnresolvedCharge)
    ));
    ledger.cancel_unstarted(&unknown.identity).unwrap();
    ledger.reserve(&plan("session", "next", 1)).unwrap();
    assert!(matches!(
        ledger.reserve(&unknown),
        Err(BudgetReservationError::IdentityConflict)
    ));
}

#[test]
fn receipt_time_ranges_exclude_future_records_and_preserve_full_u64_charges() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let mut future = plan("session", "future", 1);
    future.admitted_at = UtcTimestamp::parse("2026-09-05T00:00:00.000Z").unwrap();
    ledger
        .reconcile_accounted(&receipt(&future, 5, u64::MAX))
        .unwrap();
    ledger.reserve(&plan("session", "present", 1)).unwrap();
    let mut future_request = plan("session", "next", 1);
    future_request.admitted_at = future.admitted_at;
    assert!(matches!(
        ledger.reserve(&future_request),
        Err(BudgetReservationError::CapExceeded { used: u64::MAX, .. })
    ));
}

#[test]
fn decimal_time_index_agrees_with_linear_reference_for_multiple_days_and_corrections() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    for (index, timestamp) in [
        "2026-09-03T23:59:59.999Z",
        "2026-09-04T00:00:00.000Z",
        "2026-09-04T12:00:00.000Z",
        "2026-09-04T12:00:00.001Z",
    ]
    .iter()
    .enumerate()
    {
        let mut record = receipt(&plan("history", &format!("call-{index}"), 1), 5, 10);
        record.accounted_at = UtcTimestamp::parse(*timestamp).unwrap();
        ledger.reconcile_accounted(&record).unwrap();
    }
    let mut now = plan("new-session", "now", 80);
    ledger.reserve(&now).unwrap(); // Today through noon costs 20, not 40.
    now.identity.call_id = "overflow".into();
    now.charge = BudgetChargeBound::Bounded(BudgetCharge::UsdMicros(1));
    assert!(matches!(
        ledger.reserve(&now),
        Err(BudgetReservationError::CapExceeded {
            scope: BudgetScope::Daily,
            used: 20,
            reserved: 80,
            ..
        })
    ));
}

#[test]
fn failed_identity_and_oversized_receipt_leave_the_reservation_untouched() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let request = plan("session", "call", 90);
    ledger.reserve(&request).unwrap();
    ledger.start(&request.identity).unwrap();
    let mut wrong = receipt(&request, 5, 1);
    wrong.identity.turn_id = TurnId("different-turn".into());
    assert!(matches!(
        ledger.settle_accounted(&wrong),
        Err(BudgetReservationError::IdentityConflict)
    ));
    let mut oversized = receipt(&request, 5, 1);
    oversized.actuals.cost = Cost::Unavailable {
        reason: "x".repeat(4097),
    };
    assert!(matches!(
        ledger.settle_accounted(&oversized),
        Err(BudgetReservationError::InvalidPlan(_))
    ));
    assert!(matches!(
        ledger.reserve(&plan("session", "next", 11)),
        Err(BudgetReservationError::CapExceeded { reserved: 90, .. })
    ));
}

#[test]
fn independent_billing_units_and_correction_time_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let request = plan("session", "call", 50);
    ledger.reserve(&request).unwrap();
    ledger.start(&request.identity).unwrap();
    let mut actual = receipt(&request, 5, 0);
    actual.actuals.cost = Cost::AiCredits {
        credits_micros: 70,
        nominal_amount_micros: None,
        currency: None,
    };
    ledger.settle_accounted(&actual).unwrap();
    ledger.reserve(&plan("session", "usd", 100)).unwrap();
    let mut credits = plan("session", "credits", 0);
    credits.charge = BudgetChargeBound::Bounded(BudgetCharge::AiCreditMicros(31));
    credits.budget.session_ai_credit_cap_micros = Some(100);
    assert!(matches!(
        ledger.reserve(&credits),
        Err(BudgetReservationError::CapExceeded {
            used: 70,
            reserved: 0,
            ..
        })
    ));
    actual.sequence_id = 6.into();
    actual.accounted_at = UtcTimestamp::parse("2026-09-05T00:00:00.000Z").unwrap();
    ledger.settle_accounted(&actual).unwrap();
    ledger.reserve(&credits).unwrap(); // The corrected charge is future-dated now.
}

#[test]
fn recovery_without_a_plan_keeps_unknown_receipts_charged_as_unknown() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let mut unknown = receipt(&plan("lost", "call", 1), 5, 0);
    unknown.actuals.cost = Cost::SubscriptionQuota {
        used: None,
        unit: None,
    };
    ledger.reconcile_accounted(&unknown).unwrap();
    assert!(matches!(
        ledger.reserve(&plan("new", "next", 1)),
        Err(BudgetReservationError::UnresolvedCharge)
    ));
    unknown.sequence_id = 6.into();
    unknown.actuals.cost = Cost::SubscriptionQuota {
        used: Some("40".into()),
        unit: Some("tokens".into()),
    };
    ledger.reconcile_accounted(&unknown).unwrap();
    ledger.reserve(&plan("new", "next", 1)).unwrap();
}

#[test]
fn strict_zero_cap_also_stops_unpriced_requests() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let mut unknown = plan("session", "unknown", 0);
    unknown.charge = BudgetChargeBound::BestEffort(None);
    unknown.budget.daily_cost_cap_micros_usd = Some(0);
    assert!(matches!(
        ledger.reserve(&unknown),
        Err(BudgetReservationError::UnresolvedCharge)
    ));
}

#[test]
fn pending_recovery_pages_skip_settled_history_and_preserve_attempt_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let settled = plan("session", "older", 0);
    ledger
        .reconcile_accounted(&receipt(&settled, 5, 0))
        .unwrap();
    let mut expected = Vec::new();
    for index in 0..9 {
        let mut request = plan("session", &format!("pending-{index}"), 0);
        request.identity.attempt = index;
        ledger.reserve(&request).unwrap();
        if index % 2 == 0 {
            ledger.start(&request.identity).unwrap();
        }
        expected.push(request.identity);
    }
    ledger.reserve(&plan("different", "pending", 0)).unwrap();
    let mut cursor = None;
    let mut found = Vec::new();
    loop {
        let page = ledger
            .pending_for_session("session", cursor.as_ref(), 2)
            .unwrap();
        assert!(page.len() <= 2);
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|call| call.identity.clone());
        found.extend(page.into_iter().map(|call| call.identity));
    }
    assert_eq!(found, expected);
    let cursor = plan("different", "pending", 0).identity;
    assert!(matches!(
        ledger.pending_for_session("session", Some(&cursor), 2),
        Err(BudgetReservationError::IdentityConflict)
    ));
    assert!(ledger.pending_for_session("session", None, 129).is_err());
}

#[test]
fn parent_and_child_connections_share_one_session_cap() {
    let root = tempfile::tempdir().unwrap();
    BudgetLedger::open(root.path()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = ["parent", "child"]
        .into_iter()
        .map(|session| {
            let root = root.path().to_owned();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut ledger = BudgetLedger::open(&root).unwrap();
                let mut request = plan(session, "call", 60);
                request.identity.budget_session_id = SessionId("parent".into());
                request.budget.daily_cost_cap_micros_usd = None;
                barrier.wait();
                ledger.reserve(&request)
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(Error::CapExceeded { .. })))
            .count(),
        1
    );
}

#[test]
fn admitted_call_cannot_change_its_budget_scope() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    let mut request = plan("child", "call", 60);
    request.identity.budget_session_id = SessionId("parent".into());
    ledger.reserve(&request).unwrap();
    let mut substituted = request.identity.clone();
    substituted.budget_session_id = SessionId("other".into());
    assert!(ledger.start(&substituted).is_err());
    ledger.start(&request.identity).unwrap();
    let mut accounted = receipt(&request, 1, 17);
    accounted.identity.budget_session_id = SessionId("other".into());
    assert!(ledger.settle_accounted(&accounted).is_err());
}
