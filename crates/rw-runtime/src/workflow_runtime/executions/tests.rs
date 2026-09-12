#![allow(clippy::expect_used)]
use super::*;
use std::time::Duration;
use tokio::sync::Notify;

#[tokio::test]
async fn aborted_caller_cancels_but_waits_for_actual_effect_cleanup() {
    let executions = Arc::new(WorkflowExecutions::new());
    let cancellation = CancellationToken::default();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let work_started = Arc::clone(&started);
    let cleanup_release = Arc::clone(&release);
    let owner = Arc::clone(&executions);
    let token = cancellation.clone();
    let caller = tokio::spawn(async move {
        owner
            .run(
                token.clone(),
                Arc::new(OnceCell::new()),
                async move {
                    work_started.notify_one();
                    token.cancelled().await;
                    Err(ToolError::Command("cancelled".to_owned()))
                },
                || async move {
                    cleanup_release.notified().await;
                    Ok(())
                },
            )
            .await
    });
    started.notified().await;
    caller.abort();
    assert!(caller.await.expect_err("caller aborted").is_cancelled());
    assert!(cancellation.is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), executions.settle())
            .await
            .is_err()
    );
    assert_eq!(executions.slots.available_permits(), 3);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), executions.settle())
        .await
        .expect("cleanup settles")
        .expect("cleanup succeeded");
    assert_eq!(executions.slots.available_permits(), 4);
}

#[tokio::test]
async fn invocation_panic_releases_capacity_only_after_successful_cleanup() {
    let executions = WorkflowExecutions::new();
    let result = executions
        .run(
            CancellationToken::default(),
            Arc::new(OnceCell::new()),
            async {
                panic!("injected invocation panic");
            },
            || async { Ok(()) },
        )
        .await;
    assert!(
        result
            .expect_err("panic error")
            .to_string()
            .contains("executor panicked")
    );
    executions.settle().await.expect("cleanup settles");
    assert_eq!(executions.slots.available_permits(), 4);
}

#[tokio::test]
async fn synchronous_cleanup_panic_retains_the_unproven_obligation() {
    let executions = WorkflowExecutions::new();
    let executor = Arc::new(OnceCell::new());
    let retained = Arc::downgrade(&executor);
    let result = executions
        .run(
            CancellationToken::default(),
            executor,
            async { Err(ToolError::Command("execution failed".to_owned())) },
            || -> std::future::Ready<Result<(), ToolError>> {
                panic!("injected cleanup constructor panic");
            },
        )
        .await;
    assert!(
        result
            .expect_err("cleanup error")
            .to_string()
            .contains("cleanup panicked")
    );
    assert!(
        retained.upgrade().is_some(),
        "actual owner remains quarantined"
    );
    assert_eq!(executions.slots.available_permits(), 3);
    assert_eq!(executions.unproven.lock().expect("obligations").len(), 1);
    assert!(
        executions
            .settle()
            .await
            .expect_err("unproven cleanup")
            .to_string()
            .contains("unproven")
    );
}

#[tokio::test]
async fn normal_completion_preserves_the_parent_turn_cancellation_token() {
    let executions = WorkflowExecutions::new();
    let cancellation = CancellationToken::default();
    executions
        .run(
            cancellation.clone(),
            Arc::new(OnceCell::new()),
            async { Ok(ToolResult::new("done", serde_json::json!({}))) },
            || async { Ok(()) },
        )
        .await
        .expect("workflow completes");
    executions.settle().await.expect("settles");
    assert!(!cancellation.is_cancelled());
}

#[tokio::test]
async fn completed_unconsumed_reply_retains_its_admission_credit() {
    let executions = WorkflowExecutions::new();
    let mut caller = Box::pin(executions.run(
        CancellationToken::default(),
        Arc::new(OnceCell::new()),
        async { Ok(ToolResult::new("done", serde_json::json!({}))) },
        || async { Ok(()) },
    ));
    assert!(futures_util::poll!(&mut caller).is_pending());
    executions.settle().await.expect("effects settle");
    assert_eq!(executions.slots.available_permits(), 3);
    drop(caller);
    assert_eq!(executions.slots.available_permits(), 4);
}
