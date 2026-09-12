#![allow(clippy::expect_used)]
//! Process signals are tested in a separate child so parallel unit tests cannot
//! consume this fixture's SIGINT or manufacture an unrelated successful interrupt.
use super::PrintInterrupts;
use std::{process::Command, time::Duration};

#[test]
fn interrupt_is_retained_before_next_output_wait() {
    const CHILD: &str = "ROTTWEILER_TEST_PRINT_INTERRUPT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "headless::output::print::tests::interrupt_is_retained_before_next_output_wait",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("isolated signal fixture");
        assert!(
            result.status.success(),
            "pending interrupt child failed: {:?}; stderr: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("signal runtime")
        .block_on(async {
            let mut interrupts = PrintInterrupts::new().expect("persistent signal owner");
            for _ in 0..2 {
                rustix::process::kill_process(
                    rustix::process::getpid(),
                    rustix::process::Signal::INT,
                )
                .expect("SIGINT between print waits");
                // Let the runtime deliver the signal with no recv future alive.
                tokio::time::sleep(Duration::from_millis(20)).await;
                tokio::time::timeout(Duration::from_secs(2), interrupts.recv())
                    .await
                    .expect("signal cannot disappear between output waits")
                    .expect("pending interrupt");
            }
        });
}
