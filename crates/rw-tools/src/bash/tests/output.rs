use super::*;

#[tokio::test]
async fn copy_stream_preserves_utf8_split_across_reads() {
    let (mut writer, reader) = tokio::io::duplex(1);
    let input = "left 💩 right".as_bytes().to_vec();
    let write = tokio::spawn(async move {
        for byte in input {
            writer.write_all(&[byte]).await.expect("write byte");
        }
    });
    let sink = Arc::new(RecordingSink::default());

    copy_stream(reader, ToolOutputStream::Stdout, sink.clone())
        .await
        .expect("copy stream");
    write.await.expect("writer task");

    let rendered = sink
        .0
        .lock()
        .expect("recording")
        .iter()
        .map(|chunk| chunk.content.as_str())
        .collect::<String>();
    assert_eq!(rendered, "left 💩 right");
    assert_eq!(sink.0.lock().expect("recording").len(), 1);
}

#[tokio::test]
async fn streams_full_output_but_returns_a_tail_biased_cap() {
    let root = tempdir().expect("temp directory");
    let sink = Arc::new(RecordingSink::default());
    let context = ToolContext::new(root.path())
        .expect("context")
        .with_output(sink.clone());
    let tool = BashTool::new(
        Arc::new(StreamingExecutor),
        ToolLimits {
            max_result_bytes: 8,
            ..ToolLimits::default()
        },
    );
    let result = tool
        .execute(&context, json!({"command": "ignored"}))
        .await
        .expect("command result");
    assert!(
        tool.foreground
            .calls
            .lock()
            .expect("foreground tracking")
            .is_empty()
    );
    assert_eq!(result.data["exit_code"], 7);
    assert!(result.truncated);
    assert!(result.content.contains("89"));
    assert_eq!(sink.0.lock().expect("recording").len(), 2);
}

#[tokio::test]
async fn stdout_failure_still_awaits_stderr_settlement() {
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release, released) = tokio::sync::oneshot::channel();
    let stdout = tokio::spawn(async { Err(ToolError::Output("stdout failed".to_owned())) });
    let stderr_finished = Arc::clone(&finished);
    let stderr = tokio::spawn(async move {
        released.await.expect("release stderr");
        stderr_finished.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    });
    let mut drain = tokio::spawn(async move {
        finish_command_output(&mut CommandOutputTasks::new(stdout, stderr)).await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut drain)
            .await
            .is_err()
    );
    release.send(()).expect("release stderr");
    assert!(matches!(
        drain.await.expect("drain task"),
        Err(ToolError::Output(_))
    ));
    assert!(finished.load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test(start_paused = true)]
async fn hung_output_readers_are_aborted_after_a_bounded_drain() {
    let reader = tokio::spawn(async { std::future::pending::<Result<(), ToolError>>().await });
    let drain = tokio::spawn(async move { finish_output_task(&mut OutputTask::new(reader)).await });
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(drain.await.expect("drain join").is_ok());
}

#[test]
fn bash_declares_shared_capabilities_and_adds_write_only_for_foreground_calls() {
    let tool = BashTool::new(Arc::new(StreamingExecutor), ToolLimits::default());
    let descriptor = tool.descriptor();
    for capability in [
        ToolCapability::ReadFilesystem,
        ToolCapability::Network,
        ToolCapability::Execute,
    ] {
        assert!(descriptor.capabilities.contains(&capability));
    }
    assert!(
        !descriptor
            .capabilities
            .contains(&ToolCapability::WriteFilesystem)
    );
    assert!(
        tool.invocation_capabilities(&json!({ "command": "true" }))
            .expect("foreground capabilities")
            .contains(&ToolCapability::WriteFilesystem)
    );
    assert!(
        !tool
            .invocation_capabilities(&json!({ "command": "true", "run_in_background": true }))
            .expect("background capabilities")
            .contains(&ToolCapability::WriteFilesystem)
    );
}

#[tokio::test]
async fn cancelled_drain_keeps_output_handles_and_completed_failure_until_retry() {
    for panic_output in [false, true] {
        let (release, released) = tokio::sync::oneshot::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let stdout = tokio::spawn(async move {
            assert!(!panic_output, "controlled output task panic");
            Err(ToolError::Output("controlled output failure".to_owned()))
        });
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stderr_finished = finished.clone();
        let stderr = tokio::spawn(async move {
            released.await.expect("release actual stderr task");
            stderr_finished.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let owner = Arc::new(tokio::sync::Mutex::new(CommandOutputTasks::new(
            stdout, stderr,
        )));
        let draining = owner.clone();
        let waiter = tokio::spawn(async move {
            let mut tasks = draining.lock().await;
            started.send(()).expect("drain started");
            finish_command_output(&mut tasks).await
        });
        start.await.expect("owned drain admission");
        waiter.abort();
        assert!(
            waiter
                .await
                .expect_err("aborted cleanup waiter")
                .is_cancelled()
        );
        assert!(!finished.load(std::sync::atomic::Ordering::SeqCst));
        release
            .send(())
            .expect("actual stderr owner still retained");
        let mut tasks = owner.lock().await;
        let first = finish_command_output(&mut tasks)
            .await
            .expect_err("persisted stdout failure");
        let again = finish_command_output(&mut tasks)
            .await
            .expect_err("idempotent failed result");
        assert_eq!(first.to_string(), again.to_string());
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    }
}
