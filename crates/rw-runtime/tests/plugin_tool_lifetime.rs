//! Native acceptance uses explicit prebuilt SDK and sandbox helper artifact receipts.
//! Set `ROTTWEILER_LONG_TOOL_RECEIPT`, `ROTTWEILER_LONG_TOOL_MANIFEST` and
//! `ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT`, then run this ignored test explicitly.
#![allow(clippy::expect_used)]
use rw_plugin_protocol::{OperationLifetime, ToolCallParams};
use rw_tools::{CancellationToken, ToolError, ToolProgressSink};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[path = "plugin_tool_lifetime/fixture.rs"]
mod fixture;

#[derive(Default)]
struct Progress {
    count: AtomicUsize,
    started: tokio::sync::Notify,
}
impl ToolProgressSink for Progress {
    fn report(&self, _progress: rw_types::ToolProgress) -> Result<(), ToolError> {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.started.notify_one();
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires explicit compiled SDK and native helper receipts; includes 60 seconds of real work"]
async fn native_long_tool_progress_cancellation_and_deadline_acceptance() {
    let fixture = fixture::Fixture::load();
    for (mode, total_ms, idle_ms) in [
        ("long", 90_000, 2_000),
        ("silent", 6_000, 500),
        ("chatty", 1_800, 500),
    ] {
        let host = fixture.launch().await;
        let cancellation = CancellationToken::default();
        let progress = Arc::new(Progress::default());
        let client = host.client();
        let started = Instant::now();
        let work = tokio::spawn({
            let client = client.clone();
            let cancellation = cancellation.clone();
            let progress = progress.clone();
            async move {
                client
                    .call_tool(
                        ToolCallParams {
                            name: "work".into(),
                            input: json!({"mode":mode}),
                            lifetime: OperationLifetime::new(total_ms, idle_ms)
                                .expect("work lifetime"),
                        },
                        &cancellation,
                        progress,
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), progress.started.notified())
            .await
            .expect("native progress started");
        let mut controls = Vec::new();
        loop {
            let control_start = Instant::now();
            let reply = tokio::time::timeout(Duration::from_secs(2), client.request(
                rw_plugin_protocol::METHOD_HOOK_INVOKE,
                json!({"hook":"pre_tool","payload":{"id":"acceptance","name":"work","arguments":{}}}),
            )).await.expect("control remains responsive").expect("control reply");
            assert_eq!(reply, json!({"decision":"continue"}));
            controls.push(control_start.elapsed().as_micros());
            if mode != "long" {
                break;
            }
            if started.elapsed() >= Duration::from_mins(1) {
                cancellation.cancel();
                break;
            }
            assert!(
                !work.is_finished(),
                "long tool completed before explicit cancellation"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let terminal = tokio::time::timeout(Duration::from_secs(5), work)
            .await
            .expect("terminal proof deadline")
            .expect("tool owner")
            .expect_err("cancelled or deadline-limited tool");
        let elapsed = started.elapsed();
        if mode == "long" {
            assert_eq!(terminal.code, "cancelled");
            assert!(elapsed >= Duration::from_mins(1));
        } else {
            assert_eq!(terminal.code, "timeout");
            assert!(elapsed < Duration::from_secs(5));
        }
        if mode == "silent" {
            assert!(terminal.message.contains("idle"));
        }
        host.shutdown()
            .await
            .expect("native process and callbacks settled");
        let count = progress.count.load(Ordering::Relaxed);
        assert!(count > 0);
        assert!(
            count as u128 <= 4 + 4 * (elapsed.as_millis() / 1000 + 1),
            "progress coalescing bound"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            count,
            progress.count.load(Ordering::Relaxed),
            "progress after settlement"
        );
        println!(
            "{}",
            json!({"mode":mode,"elapsed_ms":elapsed.as_millis(),"progress":count,"control_us":controls,"terminal":terminal.code})
        );
    }
}

#[tokio::test]
#[ignore = "requires explicit compiled SDK and native helper receipts"]
async fn native_shutdown_and_relaunch_settle_each_active_tool() {
    let fixture = fixture::Fixture::load();
    for _ in 0..2 {
        let host = fixture.launch().await;
        let progress = Arc::new(Progress::default());
        let work = tokio::spawn({
            let client = host.client();
            let progress = progress.clone();
            async move {
                client
                    .call_tool(
                        ToolCallParams {
                            name: "work".into(),
                            input: json!({"mode":"long"}),
                            lifetime: OperationLifetime::new(30_000, 2_000).expect("lifetime"),
                        },
                        &CancellationToken::default(),
                        progress,
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), progress.started.notified())
            .await
            .expect("active native tool");
        assert!(!work.is_finished());
        tokio::time::timeout(Duration::from_secs(5), host.shutdown())
            .await
            .expect("native shutdown deadline")
            .expect("physical shutdown proof");
        tokio::time::timeout(Duration::from_secs(5), work)
            .await
            .expect("request owner released after shutdown")
            .expect("request job")
            .expect_err("shutdown rejects active operation");
        let count = progress.count.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(progress.count.load(Ordering::Relaxed), count);
    }
}
