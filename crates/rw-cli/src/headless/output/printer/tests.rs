#![allow(clippy::expect_used)]
use super::{OwnedPrinter, PRINT_SCOPE_BYTES, PrinterScope};
use rustyline::ExternalPrinter;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::sync::{Semaphore, oneshot};

struct HeldPrinter {
    entered: Option<oneshot::Sender<()>>,
    release: mpsc::Receiver<()>,
    dropped: Arc<AtomicBool>,
}
impl ExternalPrinter for HeldPrinter {
    fn print(&mut self, _message: String) -> rustyline::Result<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        self.release
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(())
    }
}
impl Drop for HeldPrinter {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn cancelled_print_retains_physical_worker_and_byte_envelope() {
    let budget = Arc::new(Semaphore::new(PRINT_SCOPE_BYTES));
    let scope = PrinterScope::acquire_from(budget.clone()).expect("scope");
    let (entered, started) = oneshot::channel();
    let (release, receive) = mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let mut printer = OwnedPrinter::new(
        Box::new(HeldPrinter {
            entered: Some(entered),
            release: receive,
            dropped: dropped.clone(),
        }),
        scope,
    );
    let caller = tokio::spawn(async move { printer.print("ordered output".to_owned()).await });
    started.await.expect("physical worker entered");
    // This current-thread runtime still runs while the real printer is blocked.
    assert!(!caller.is_finished());
    caller.abort();
    assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    assert!(!dropped.load(Ordering::Acquire));
    assert_eq!(budget.available_permits(), 0);
    assert!(PrinterScope::acquire_from(budget.clone()).is_err());
    release.send(()).expect("release physical write");
    tokio::time::timeout(Duration::from_secs(5), async {
        while budget.available_permits() != PRINT_SCOPE_BYTES {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker releases after actual settlement");
    assert!(dropped.load(Ordering::Acquire));
}

struct RecordingPrinter(Arc<Mutex<Vec<String>>>);
impl ExternalPrinter for RecordingPrinter {
    fn print(&mut self, message: String) -> rustyline::Result<()> {
        if message == "failed" {
            return Err(io::Error::other("terminal closed").into());
        }
        self.0.lock().expect("recorded output").push(message);
        Ok(())
    }
}

#[tokio::test]
async fn ordered_prints_preserve_errors_and_reader_retains_queued_bytes() {
    let budget = Arc::new(Semaphore::new(PRINT_SCOPE_BYTES));
    let scope = PrinterScope::acquire_from(budget.clone()).expect("scope");
    let reader = scope.clone();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut printer = OwnedPrinter::new(Box::new(RecordingPrinter(recorded.clone())), scope);
    printer
        .print("question".to_owned())
        .await
        .expect("question");
    printer
        .print("permission".to_owned())
        .await
        .expect("permission");
    assert!(
        printer
            .print("failed".to_owned())
            .await
            .expect_err("write error")
            .to_string()
            .contains("terminal closed")
    );
    printer
        .print("next event".to_owned())
        .await
        .expect("owner returned after error");
    assert_eq!(
        *recorded.lock().expect("recorded"),
        ["question", "permission", "next event"]
    );
    drop(printer);
    assert_eq!(budget.available_permits(), 0);
    drop(reader);
    assert_eq!(budget.available_permits(), PRINT_SCOPE_BYTES);
}

#[tokio::test]
async fn oversized_capacity_is_rejected_before_printer_transfer() {
    let budget = Arc::new(Semaphore::new(PRINT_SCOPE_BYTES));
    let scope = PrinterScope::acquire_from(budget).expect("scope");
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut printer = OwnedPrinter::new(Box::new(RecordingPrinter(recorded.clone())), scope);
    let oversized = String::with_capacity(super::MAX_REPL_OUTPUT_BYTES + 1);
    assert!(printer.print(oversized).await.is_err());
    printer
        .print("still owned".to_owned())
        .await
        .expect("prior printer survives refusal");
    assert_eq!(*recorded.lock().expect("recorded"), ["still owned"]);
}
