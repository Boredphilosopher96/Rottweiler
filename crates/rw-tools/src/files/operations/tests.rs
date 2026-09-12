#![allow(clippy::expect_used)]
use super::{FileOperations, FileTransaction};
use crate::{ToolContext, ToolError};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::tempdir;
use tokio::sync::oneshot;

#[tokio::test]
async fn successful_reply_does_not_cancel_its_context() {
    let root = tempdir().expect("root");
    let context = ToolContext::new(root.path()).expect("context");
    let operations = FileOperations::new();
    assert_eq!(
        operations
            .run(context.clone(), |_, _| Ok(42))
            .await
            .expect("result"),
        42
    );
    context.cancellation.check().expect("parent remains usable");
    operations.settle().await.expect("settled");
}

#[tokio::test]
async fn abandoned_blocked_worker_retains_admission_until_it_really_finishes() {
    let root = tempdir().expect("root");
    let context = ToolContext::new(root.path()).expect("context");
    let operations = FileOperations::with_limits(1, Duration::from_millis(25));
    let (started, began) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let worker_operations = operations.clone();
    let worker_context = context.clone();
    let caller = tokio::spawn(async move {
        worker_operations
            .run(worker_context, move |context, _| {
                let _ = started.send(());
                blocked.recv().expect("released");
                context.cancellation.check()?;
                std::fs::write(
                    context
                        .resolve_writable(std::path::Path::new("late.txt"))
                        .expect("path"),
                    "late",
                )
                .expect("write");
                Ok(())
            })
            .await
    });
    began.await.expect("worker started");
    operations
        .settle()
        .await
        .expect("unrelated live caller is not abandoned");
    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());
    assert!(matches!(
        operations.settle().await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert_eq!(operations.0.admission.available_permits(), 0);
    release.send(()).expect("release actual worker");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if operations.0.calls.lock().expect("calls").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker joined");
    operations.settle().await.expect("proof now complete");
    assert_eq!(operations.0.admission.available_permits(), 1);
    assert!(!root.path().join("late.txt").exists());
}

#[tokio::test]
async fn unconsumed_completed_reply_keeps_its_result_credit() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let root = tempdir().expect("root");
    let context = ToolContext::new(root.path()).expect("context");
    let operations = FileOperations::with_limits(1, Duration::from_secs(1));
    let mut future = Box::pin(operations.run(context.clone(), |_, _| Ok(vec![1u8; 4096])));
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if operations.0.calls.lock().expect("calls").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completion delivered");
    assert_eq!(operations.0.admission.available_permits(), 0);
    assert!(matches!(
        operations.run(context, |_, _| Ok(())).await,
        Err(ToolError::Command(_))
    ));
    drop(future);
    assert_eq!(operations.0.admission.available_permits(), 1);
}

#[cfg(unix)]
fn temporary(context: &ToolContext, transaction: &mut FileTransaction) -> std::path::PathBuf {
    let path = context
        .resolve_writable(std::path::Path::new("owned.tmp"))
        .expect("path");
    let (parent, name) = context.secure_parent(&path).expect("parent");
    std::fs::write(&path, "pending").expect("temporary");
    transaction.register(parent, name, path.clone());
    path
}

#[cfg(unix)]
#[tokio::test]
async fn panicking_operation_rolls_back_its_registered_temporary() {
    let root = tempdir().expect("root");
    let context = ToolContext::new(root.path()).expect("context");
    let operations = FileOperations::new();
    let result = operations
        .run(
            context.clone(),
            |context, transaction| -> Result<(), ToolError> {
                temporary(context, transaction);
                panic!("injected file operation panic");
            },
        )
        .await;
    assert!(matches!(result, Err(ToolError::Command(_))));
    assert!(!root.path().join("owned.tmp").exists());
    operations.settle().await.expect("panic cleanup proven");
    context
        .cancellation
        .check()
        .expect("normal error response does not cancel parent");
}

#[cfg(unix)]
#[tokio::test]
async fn failed_temporary_cleanup_retains_its_owner_and_closes_admission() {
    let root = tempdir().expect("root");
    let context = ToolContext::new(root.path()).expect("context");
    let operations = FileOperations::with_limits(1, Duration::from_secs(1));
    let retained = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&retained);
    let result = operations
        .run(context, move |context, transaction| {
            let path = temporary(context, transaction);
            std::fs::remove_file(&path).expect("replace temporary");
            std::fs::create_dir(&path).expect("inject unlink failure");
            *captured.lock().expect("path") = Some(path);
            Ok(())
        })
        .await;
    assert!(matches!(result, Err(ToolError::EffectsUnsettled(_))));
    assert!(matches!(
        operations.settle().await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert_eq!(operations.0.calls.lock().expect("retained calls").len(), 1);
    assert_eq!(operations.0.admission.available_permits(), 0);
    assert!(
        retained
            .lock()
            .expect("path")
            .as_ref()
            .expect("retained")
            .is_dir()
    );
}
