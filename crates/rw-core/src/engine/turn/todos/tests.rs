#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{ActorTodoStore, MAX_REQUESTS};
use crate::engine::turn::signals::TurnSignal;
use rw_tools::{
    CancellationToken, TodoAction, TodoAdmission, TodoStateStore, ToolError, ToolResult,
};
use std::sync::Arc;
use tokio::sync::mpsc;
fn admission() -> TodoAdmission {
    TodoAdmission {
        max_items: 128,
        max_state_bytes: 65536,
        max_result_bytes: 65536,
    }
}

#[tokio::test]
async fn abandoned_task_request_retains_credit_until_actor_acknowledges() {
    let (send, mut receive) = mpsc::unbounded_channel();
    let store = Arc::new(ActorTodoStore::new(1, send));
    let call_store = Arc::clone(&store);
    let call = tokio::spawn(async move {
        call_store
            .transact(
                TodoAction::Clear {},
                admission(),
                CancellationToken::default(),
            )
            .await
    });
    let TurnSignal::Todo(mut request) = receive.recv().await.expect("request") else {
        panic!("task request")
    };
    call.abort();
    let _ = call.await;
    assert_eq!(store.credits.available_permits(), MAX_REQUESTS - 1);
    let proof_store = Arc::clone(&store);
    let proof = tokio::spawn(async move { proof_store.settle_effects().await });
    tokio::task::yield_now().await;
    assert!(!proof.is_finished());
    request.finish(Ok(ToolResult::new("done", serde_json::json!({}))), None);
    proof.await.expect("proof join").expect("proof");
    assert_eq!(
        store.credits.available_permits(),
        MAX_REQUESTS - 1,
        "actor still owns request allocation"
    );
    drop(request);
    assert_eq!(store.credits.available_permits(), MAX_REQUESTS);
}
#[tokio::test]
async fn lost_actor_request_retains_failed_owner_and_credit() {
    let (send, mut receive) = mpsc::unbounded_channel();
    let store = Arc::new(ActorTodoStore::new(1, send));
    let call_store = Arc::clone(&store);
    let call = tokio::spawn(async move {
        call_store
            .transact(
                TodoAction::Clear {},
                admission(),
                CancellationToken::default(),
            )
            .await
    });
    drop(receive.recv().await.expect("request"));
    assert!(matches!(
        call.await.expect("join"),
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert!(store.settle_effects().await.is_err());
    assert_eq!(store.credits.available_permits(), MAX_REQUESTS - 1);
}
#[tokio::test(start_paused = true)]
async fn late_acknowledgement_cannot_clear_a_failed_settlement_proof() {
    let (send, mut receive) = mpsc::unbounded_channel();
    let store = Arc::new(ActorTodoStore::new(1, send));
    let call_store = Arc::clone(&store);
    let call = tokio::spawn(async move {
        call_store
            .transact(
                TodoAction::Clear {},
                admission(),
                CancellationToken::default(),
            )
            .await
    });
    let TurnSignal::Todo(mut request) = receive.recv().await.expect("request") else {
        panic!("task request")
    };
    call.abort();
    let _ = call.await;
    assert!(store.settle_effects().await.is_err());
    request.finish(Ok(ToolResult::new("late", serde_json::json!({}))), None);
    drop(request);
    assert!(store.settle_effects().await.is_err());
    assert_eq!(store.credits.available_permits(), MAX_REQUESTS - 1);
}
