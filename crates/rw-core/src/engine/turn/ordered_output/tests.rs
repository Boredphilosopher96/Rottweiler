#![cfg(test)]
#![allow(clippy::expect_used)]
use crate::NoopSecretRedactor;
use std::time::Duration;

use super::{OrderedOutputCoordinator, OrderedOutputSink};
use crate::engine::pending_event::PendingEvent;
use crate::engine::turn::signals::TurnSignal;
use crate::engine::{MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS, MAX_LIVE_TOOL_OUTPUT_BYTES};
use rw_tools::{CancellationToken, ToolError, ToolOutputChunk, ToolOutputSink};
use rw_types::ToolOutputStream;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn chunk(content: impl Into<String>) -> ToolOutputChunk {
    ToolOutputChunk {
        stream: ToolOutputStream::Stdout,
        content: content.into(),
    }
}

#[tokio::test]
async fn promotion_bypasses_saturated_background_capacity_without_exceeding_global_bound() {
    let (signals, mut receiver) = mpsc::unbounded_channel();
    let coordinator = OrderedOutputCoordinator::new(1, signals, Arc::new(NoopSecretRedactor));
    for index in 0..MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS - 1 {
        coordinator
            .emit(
                2,
                "later",
                &rw_types::ToolInvocationId("later".to_owned()),
                chunk(index.to_string()),
            )
            .await
            .expect("buffer later");
    }
    let promoted_id = rw_types::ToolInvocationId("promoted".to_owned());
    let promoted = coordinator.emit(1, "promoted", &promoted_id, chunk("next"));
    tokio::pin!(promoted);
    assert!(futures_util::poll!(&mut promoted).is_pending());
    coordinator
        .emit(
            0,
            "first",
            &rw_types::ToolInvocationId("first".to_owned()),
            chunk("current"),
        )
        .await
        .expect("reserved slot");
    assert_eq!(coordinator.permits.available_permits(), 0);
    assert_eq!(receiver.len(), 1);
    {
        let first_id = rw_types::ToolInvocationId("first".to_owned());
        let blocked = coordinator.emit(0, "first", &first_id, chunk("extra"));
        tokio::pin!(blocked);
        assert!(futures_util::poll!(&mut blocked).is_pending());
    }
    drop(receiver.recv().await.expect("first chunk"));
    coordinator.advance(1);
    tokio::time::timeout(Duration::from_secs(1), &mut promoted)
        .await
        .expect("promotion must wake background waiter")
        .expect("promoted emit");
    assert!(
        matches!(receiver.recv().await, Some(TurnSignal::ToolOutput {
        event: PendingEvent::ToolOutput { id, .. }, ..
    }) if id == "promoted")
    );
    coordinator.advance(2);
    for index in 0..MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS - 1 {
        assert!(
            matches!(receiver.recv().await, Some(TurnSignal::ToolOutput {
            event: PendingEvent::ToolOutput { id, chunk, .. }, ..
        }) if id == "later" && chunk == index.to_string())
        );
    }
    assert_eq!(
        coordinator.permits.available_permits(),
        MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS
    );
    assert_eq!(
        coordinator.background_permits.available_permits(),
        MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS - 1
    );
    assert!(
        coordinator
            .emit(
                0,
                "late",
                &rw_types::ToolInvocationId("late".to_owned()),
                chunk("stale")
            )
            .await
            .is_err()
    );
    drop(receiver);
    assert!(
        coordinator
            .emit(
                2,
                "closed",
                &rw_types::ToolInvocationId("closed".to_owned()),
                chunk("gone")
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cancellation_releases_blocked_output_without_waiting_for_promotion() {
    let (signals, _receiver) = mpsc::unbounded_channel();
    let coordinator = Arc::new(OrderedOutputCoordinator::new(
        1,
        signals,
        Arc::new(NoopSecretRedactor),
    ));
    for _ in 0..MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS - 1 {
        coordinator
            .emit(
                2,
                "later",
                &rw_types::ToolInvocationId("later".to_owned()),
                chunk("buffered"),
            )
            .await
            .expect("buffer later");
    }
    let cancellation = CancellationToken::default();
    let sink = OrderedOutputSink {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        index: 1,
        id: "cancelled".to_owned(),
        coordinator: Arc::clone(&coordinator),
        open: Arc::new(AtomicBool::new(true)),
        cancellation: cancellation.clone(),
        totals: Mutex::new((0, 0, false)),
    };
    let waiting = sink.emit(chunk("blocked"));
    tokio::pin!(waiting);
    assert!(futures_util::poll!(&mut waiting).is_pending());
    cancellation.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("cancel output wait"),
        Err(ToolError::Cancelled)
    ));
    assert_eq!(coordinator.permits.available_permits(), 1);
    assert!(
        !coordinator
            .state
            .lock()
            .expect("state")
            .buffered
            .contains_key(&1)
    );
}

#[tokio::test]
async fn oversized_chunks_preserve_the_live_output_byte_ceiling() {
    let (signals, mut receiver) = mpsc::unbounded_channel();
    let sink = OrderedOutputSink {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        index: 0,
        id: "large".to_owned(),
        coordinator: Arc::new(OrderedOutputCoordinator::new(
            1,
            signals,
            Arc::new(NoopSecretRedactor),
        )),
        open: Arc::new(AtomicBool::new(true)),
        cancellation: CancellationToken::default(),
        totals: Mutex::new((0, 0, false)),
    };
    sink.emit(chunk("界".repeat(MAX_LIVE_TOOL_OUTPUT_BYTES)))
        .await
        .expect("truncate oversized chunk");
    assert!(
        matches!(receiver.recv().await, Some(TurnSignal::ToolOutput {
        event: PendingEvent::ToolOutput { chunk, .. }, ..
    }) if chunk.starts_with("[live tool output truncated;") && chunk.len() < 100)
    );
    sink.emit(chunk("discarded after truncation"))
        .await
        .expect("keep draining");
    assert!(receiver.try_recv().is_err());
}
