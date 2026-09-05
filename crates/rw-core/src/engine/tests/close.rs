use super::*;

#[derive(Default)]
struct CleanupTool {
    entered: Notify,
    release: Notify,
    completed: AtomicBool,
    callback: Mutex<Option<SessionHandle>>,
}

#[async_trait]
impl Tool for CleanupTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "cleanup_probe".to_owned(),
            description: "owned cleanup fixture".to_owned(),
            input_schema: json!({"type":"object"}),
            capabilities: CapabilityManifest::new(Vec::new()),
        }
    }
    async fn execute(&self, _: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::new("done", Value::Null))
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }
    async fn end_session(&self, _: &SessionId) -> Result<(), ToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        let callback = self.callback.lock().expect("callback").take();
        if let Some(handle) = callback {
            handle
                .record_subagent_spawned(
                    SubagentId("cleanup-child".to_owned()),
                    SessionId("cleanup-child-session".to_owned()),
                    "closing child acknowledgement".to_owned(),
                )
                .await
                .map_err(|error| ToolError::EffectsUnsettled(error.to_string()))?;
        }
        self.completed.store(true, Ordering::Release);
        Ok(())
    }
}

#[tokio::test]
async fn dropped_close_caller_does_not_release_cleanup_or_block_internal_acknowledgements() {
    let root = tempfile::tempdir().expect("root");
    let tool = Arc::new(CleanupTool::default());
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("registered");
    let handle = SessionActor::spawn(config(
        root.path(),
        Arc::new(ScriptedModel::new(Vec::new())),
        Arc::new(tools),
        PermissionDecision::Allow,
        HookDispatcher::new(),
    ))
    .expect("actor");
    *tool.callback.lock().expect("callback") = Some(handle.clone());
    let closing = tokio::spawn({
        let handle = handle.clone();
        async move { handle.close().await }
    });
    tool.entered.notified().await;
    assert!(!closing.is_finished());
    closing.abort();
    assert!(closing.await.expect_err("caller aborted").is_cancelled());
    assert!(!tool.completed.load(Ordering::Acquire));
    tool.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), handle.close())
        .await
        .expect("cleanup ack")
        .expect("settled");
    assert!(tool.completed.load(Ordering::Acquire));
    assert!(handle.send_message("late mutation").await.is_err());
    handle.close().await.expect("idempotent closed proof");
}

struct FailedModel {
    entered: Notify,
    panic: bool,
}
#[async_trait]
impl ModelDriver for FailedModel {
    fn stream(
        &self,
        _: &str,
        _: ProviderRequest,
        _: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.entered.notify_one();
        Ok(Box::pin(stream::iter(stop_script("response", &[]))))
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        assert!(!self.panic, "fixture model settlement panic");
        Err(AgentLoopError::EffectsUnsettled(
            "fixture model effects remain owned".to_owned(),
        ))
    }
}

#[tokio::test]
async fn failed_or_panicked_model_cleanup_has_no_terminal_and_retains_owner_after_close() {
    for panic in [false, true] {
        let root = tempfile::tempdir().expect("root");
        let model = Arc::new(FailedModel {
            entered: Notify::new(),
            panic,
        });
        let weak = Arc::downgrade(&model);
        let sink = Arc::new(RecordingSink::default());
        let mut configuration = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            HookDispatcher::new(),
        );
        configuration.event_sink = sink.clone();
        let handle = SessionActor::spawn(configuration).expect("actor");
        handle
            .send_message("trigger invocation")
            .await
            .expect("turn admitted");
        model.entered.notified().await;
        let proof = tokio::time::timeout(Duration::from_secs(1), handle.close())
            .await
            .expect("failed proof is bounded");
        assert!(matches!(proof, Err(AgentLoopError::EffectsUnsettled(_))));
        assert!(
            sink.events
                .lock()
                .expect("events")
                .iter()
                .all(|event| !matches!(event.kind, PendingEvent::TurnFinished { .. }))
        );
        drop(model);
        drop(handle);
        tokio::task::yield_now().await;
        assert!(weak.upgrade().is_some());
    }
}
