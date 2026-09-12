//! Independent `SQLite` clients must retain liabilities across abrupt process exit.
use super::*;
use rw_resources::process::BlockingProcess;
use std::{
    path::Path,
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const ROOT_ENV: &str = "RW_RESERVATION_PROCESS_TEST_ROOT";
const INDEX_ENV: &str = "RW_RESERVATION_PROCESS_TEST_INDEX";
const TEST: &str = "session::reservations::tests::process::independent_processes_cannot_overspend_or_refund_a_crashed_call";

fn await_condition(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "reservation process deadline");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn child(root: &Path, index: usize) -> ! {
    let mut ledger = BudgetLedger::open(root).unwrap();
    std::fs::write(root.join(format!("ready-{index}")), b"ready").unwrap();
    await_condition(|| std::fs::read(root.join("release")).is_ok_and(|v| v == b"release"));
    let request = plan(&format!("session-{index}"), "call", 80);
    let exit = match ledger.reserve(&request) {
        Ok(()) => {
            ledger.start(&request.identity).unwrap();
            77
        }
        Err(BudgetReservationError::CapExceeded {
            scope: BudgetScope::Daily,
            ..
        }) => 78,
        other => panic!("unexpected reservation outcome: {other:?}"),
    };
    // Bypass every destructor, including the connection and reservation owner.
    std::process::exit(exit)
}

fn settled_status(process: &mut BlockingProcess) -> ExitStatus {
    let mut status = None;
    await_condition(|| {
        status = process.try_status().unwrap();
        status.is_some()
    });
    process.settle();
    status.unwrap()
}

#[test]
fn independent_processes_cannot_overspend_or_refund_a_crashed_call() {
    if let Some(root) = std::env::var_os(ROOT_ENV) {
        child(
            Path::new(&root),
            std::env::var(INDEX_ENV).unwrap().parse().unwrap(),
        );
    }
    let root = tempfile::tempdir().unwrap();
    drop(BudgetLedger::open(root.path()).unwrap());
    // Declare owners after scratch: even an assertion panic retires children
    // before the database directory is removed.
    let mut workers: Vec<_> = (0..2)
        .map(|index| {
            BlockingProcess::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(ROOT_ENV, root.path())
                    .env(INDEX_ENV, index.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null()),
            )
            .unwrap()
        })
        .collect();
    await_condition(|| {
        (0..2).all(|index| {
            std::fs::read(root.path().join(format!("ready-{index}")))
                .is_ok_and(|bytes| bytes == b"ready")
        })
    });
    std::fs::write(root.path().join("release"), b"release").unwrap();
    let statuses: Vec<_> = workers.iter_mut().map(settled_status).collect();
    assert_eq!(statuses.iter().filter(|s| s.code() == Some(77)).count(), 1);
    assert_eq!(statuses.iter().filter(|s| s.code() == Some(78)).count(), 1);
    let winner = statuses.iter().position(|s| s.code() == Some(77)).unwrap();
    let request = plan(&format!("session-{winner}"), "call", 80);
    let mut ledger = BudgetLedger::open(root.path()).unwrap();
    assert_eq!(
        ledger.phase(&request.identity).unwrap(),
        Some(ProviderCallPhase::Started)
    );
    assert!(matches!(
        ledger.cancel_unstarted(&request.identity),
        Err(BudgetReservationError::IdentityConflict)
    ));
    assert!(matches!(
        ledger.start(&request.identity),
        Err(BudgetReservationError::IdentityConflict)
    ));
    assert!(matches!(
        ledger.reserve(&plan("fresh-session", "next", 21)),
        Err(BudgetReservationError::CapExceeded {
            scope: BudgetScope::Daily,
            reserved: 80,
            ..
        })
    ));
    ledger.settle_accounted(&receipt(&request, 5, 30)).unwrap();
    ledger.reserve(&plan("fresh-session", "next", 70)).unwrap();
}
