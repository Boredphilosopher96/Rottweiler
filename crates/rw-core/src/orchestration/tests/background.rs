//! Background-by-default child agents: status, delivery, locking, queueing, and detaching.
use super::*;

struct Fixture {
    _workspace: tempfile::TempDir,
    owner: SubagentOrchestrator,
    tool: SpawnAgentTool,
    sink: Arc<RecordingSubagentSink>,
    context: ToolContext,
}

fn fixture(limits: SubagentLimits) -> Fixture {
    let workspace = tempfile::tempdir().expect("workspace");
    let owner = orchestrator(limits, Arc::new(FakeFactory::default()));
    let mut agents =
        rw_ext::compose_agent_registry(&rw_ext::ExtensionCatalog::default()).expect("agents");
    agents
        .resolve_tool_names(std::iter::empty())
        .expect("tools");
    let tool = SpawnAgentTool::new(
        owner.clone(),
        Arc::new(agents),
        Arc::new(|| Ok(Arc::new(SelectedModel) as Arc<dyn crate::ModelDriver>)),
    );
    let sink = Arc::new(RecordingSubagentSink::default());
    let context = ToolContext::new(workspace.path())
        .expect("context")
        .with_session_id(parent())
        .with_model_alias("openai_codex/gpt-5.6-sol")
        .with_background_subagent_event_sink(sink.clone());
    Fixture {
        _workspace: workspace,
        owner,
        tool,
        sink,
        context,
    }
}

fn parent() -> SessionId {
    SessionId("parent".into())
}

fn lifecycle_count(sink: &RecordingSubagentSink) -> usize {
    sink.lifecycles
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

#[tokio::test]
async fn spawn_runs_in_the_background_by_default_and_wakes_the_parent_when_done() {
    let fixture = fixture(SubagentLimits::default());
    let started = tokio::time::timeout(
        Duration::from_millis(100),
        fixture.tool.execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:250","agent":"explore"}),
        ),
    )
    .await
    .expect("spawn must not await the child")
    .expect("spawn");
    assert_eq!(started.data["status"], "running");
    assert_eq!(started.data["background"], true);
    assert!(started.content.contains("delivered to you automatically"));
    let id = started.data["id"].clone();
    let listed = fixture
        .tool
        .execute(&fixture.context, json!({"action":"list"}))
        .await
        .expect("list");
    assert_eq!(listed.data["children"][0]["state"], "running");
    assert_eq!(listed.data["children"][0]["agent"], "explore");
    assert!(
        fixture.tool.session_activity(&parent()).is_none(),
        "a worktree child never holds the parent workspace lock"
    );
    let stranger = ToolContext::new(fixture.context.workspace_root())
        .expect("stranger")
        .with_session_id(SessionId("other-parent".into()));
    assert!(
        fixture
            .tool
            .execute(&stranger, json!({"action":"wait","ids":[id]}))
            .await
            .is_err()
    );
    // The invoking turn ends; the session-owned child keeps running.
    fixture.context.cancellation.cancel();
    let next_turn = ToolContext::new(fixture.context.workspace_root())
        .expect("next turn")
        .with_session_id(parent());
    let waited = fixture
        .tool
        .execute(&next_turn, json!({"action":"wait","ids":[id]}))
        .await
        .expect("wait");
    assert_eq!(waited.data["children"][0]["status"], "completed");
    assert!(waited.content.contains("<child-agent-result>"));
    assert_eq!(lifecycle_count(&fixture.sink), 2);
    assert_eq!(
        fixture.sink.wakes.load(Ordering::SeqCst),
        1,
        "a background completion asks to wake an idle parent"
    );
    fixture.tool.end_session(&parent()).await.expect("close");
    assert!(fixture.owner.list_for_parent(&parent()).is_empty());
}

#[tokio::test]
async fn only_an_editing_shared_child_holds_the_workspace_lock() {
    let fixture = fixture(SubagentLimits::default());
    fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:10000","agent":"explore","isolation":"shared"}),
        )
        .await
        .expect("read-only shared child");
    assert!(fixture.tool.session_activity(&parent()).is_none());
    fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:10000","agent":"general"}),
        )
        .await
        .expect("editing worktree child");
    assert!(fixture.tool.session_activity(&parent()).is_none());
    fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:10000","agent":"general","isolation":"shared"}),
        )
        .await
        .expect("editing shared child");
    let activity = fixture
        .tool
        .session_activity(&parent())
        .expect("editing shared child holds the lock");
    assert_eq!(activity, rw_tools::SessionActivity::SharedWorkspaceChild);
    assert!(
        activity
            .blocked("workspace mutation")
            .contains("a child agent is editing the shared workspace")
    );
    tokio::time::timeout(Duration::from_secs(2), fixture.tool.end_session(&parent()))
        .await
        .expect("shutdown bounded")
        .expect("shutdown settles children");
    assert!(fixture.tool.session_activity(&parent()).is_none());
    let events = fixture.sink.lifecycles.lock().expect("events");
    assert!(
        matches!(events.last(), Some(SubagentLifecycleEvent::Finished { result, .. }) if result.status == SubagentStatus::Cancelled)
    );
}

#[tokio::test]
async fn moving_a_foreground_child_to_the_background_releases_its_waiting_call() {
    let fixture = fixture(SubagentLimits::default());
    let tool = Arc::new(fixture.tool);
    let context = fixture.context.clone();
    let waiting = {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move {
            tool.execute(
                &context,
                json!({"action":"spawn","task":"delay:400","agent":"explore","background":false}),
            )
            .await
        })
    };
    let id = loop {
        if let Some(child) = fixture.owner.list_for_parent(&parent()).pop() {
            break child.subagent_id;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert!(matches!(
        fixture
            .owner
            .move_to_background(&SessionId("stranger".into()), &id),
        Err(OrchestrationError::UnknownSubagent(_))
    ));
    fixture
        .owner
        .move_to_background(&parent(), &id)
        .expect("foreground child detaches");
    let released = tokio::time::timeout(Duration::from_millis(200), waiting)
        .await
        .expect("the blocked spawn resolves at once")
        .expect("join")
        .expect("spawn");
    assert_eq!(released.data["background"], true);
    assert!(released.content.contains("moved to the background"));
    assert!(matches!(
        fixture.owner.move_to_background(&parent(), &id),
        Err(OrchestrationError::NotInForeground(_))
    ));
    let finished = fixture
        .owner
        .wait_for_parent(&parent(), &id)
        .await
        .expect("child still completes");
    assert!(
        matches!(finished, ChildWait::Finished(result) if result.status == SubagentStatus::Completed)
    );
    assert_eq!(
        fixture.sink.wakes.load(Ordering::SeqCst),
        1,
        "a detached child is delivered like any background completion"
    );
    tool.end_session(&parent()).await.expect("close");
}

#[tokio::test]
async fn a_foreground_child_finishes_inside_its_call_without_waking_the_parent() {
    let fixture = fixture(SubagentLimits::default());
    let finished = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:1","agent":"explore","background":false}),
        )
        .await
        .expect("foreground spawn");
    assert_eq!(finished.data["status"], "completed");
    assert_eq!(fixture.sink.wakes.load(Ordering::SeqCst), 0);
    let id = finished.data["id"].clone();
    let continued = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"message","id":id,"message":"delay:1"}),
        )
        .await
        .expect("background message");
    assert_eq!(continued.data["status"], "running");
    let waited = fixture
        .tool
        .execute(&fixture.context, json!({"action":"wait","ids":[id]}))
        .await
        .expect("wait");
    assert_eq!(waited.data["children"][0]["status"], "completed");
    assert_eq!(fixture.sink.wakes.load(Ordering::SeqCst), 1);
    fixture.tool.end_session(&parent()).await.expect("close");
}

#[tokio::test]
async fn spawns_beyond_the_limit_are_queued_and_wait_can_time_out() {
    let fixture = fixture(SubagentLimits {
        max_concurrency: 1,
        ..SubagentLimits::default()
    });
    let first = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:300","agent":"explore"}),
        )
        .await
        .expect("first");
    assert_eq!(first.data["status"], "running");
    let second = tokio::time::timeout(
        Duration::from_millis(100),
        fixture.tool.execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:1","agent":"explore"}),
        ),
    )
    .await
    .expect("a full slot never blocks the parent")
    .expect("queued spawn");
    assert_eq!(second.data["status"], "queued");
    assert!(second.content.contains("Queued child"));
    let listed = fixture
        .tool
        .execute(&fixture.context, json!({"action":"list"}))
        .await
        .expect("list");
    let states = listed.data["children"]
        .as_array()
        .expect("children")
        .iter()
        .map(|child| child["state"].as_str().expect("state").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(states, ["running", "queued"]);
    let timed_out = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"wait","ids":[second.data["id"]],"timeout_seconds":1}),
        )
        .await
        .expect("wait for the queued child");
    assert_eq!(timed_out.data["children"][0]["status"], "completed");
    assert!(
        fixture
            .tool
            .execute(
                &fixture.context,
                json!({"action":"wait","ids":[first.data["id"]],"timeout_seconds":0}),
            )
            .await
            .is_err()
    );
    fixture.tool.end_session(&parent()).await.expect("close");
}

#[tokio::test]
async fn wake_on_completion_can_be_disabled() {
    let fixture = fixture(SubagentLimits {
        wake_on_completion: false,
        ..SubagentLimits::default()
    });
    let started = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:1","agent":"explore"}),
        )
        .await
        .expect("spawn");
    fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"wait","ids":[started.data["id"]]}),
        )
        .await
        .expect("wait");
    assert_eq!(lifecycle_count(&fixture.sink), 2);
    assert_eq!(fixture.sink.wakes.load(Ordering::SeqCst), 0);
    fixture.tool.end_session(&parent()).await.expect("close");
}

#[tokio::test]
async fn listing_reports_queued_children_and_queued_continuations() {
    let fixture = fixture(SubagentLimits {
        max_concurrency: 1,
        ..SubagentLimits::default()
    });
    let finished = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:1","agent":"explore","background":false}),
        )
        .await
        .expect("idle child");
    let idle = SubagentId(finished.data["id"].as_str().expect("id").to_owned());
    fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:300","agent":"explore"}),
        )
        .await
        .expect("running child");
    let queued = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:1","agent":"explore"}),
        )
        .await
        .expect("queued child");
    assert_eq!(queued.data["status"], "queued");
    fixture
        .owner
        .continue_child(
            &parent(),
            &idle,
            "delay:1".to_owned(),
            fixture.owner.background_observer(fixture.sink.clone()),
            CancellationToken::default(),
        )
        .await
        .expect("continuation waits for a slot");
    let activities = fixture
        .owner
        .list_for_parent(&parent())
        .into_iter()
        .map(|child| child.activity)
        .collect::<Vec<_>>();
    assert_eq!(
        activities,
        [
            SubagentActivity::Queued,
            SubagentActivity::Running,
            SubagentActivity::Queued,
        ],
        "the protocol listing includes children waiting for a slot"
    );
    assert!(fixture.owner.has_outstanding_children(&parent()));
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.owner.children_settled(&parent()),
    )
    .await
    .expect("children settle once each has run");
    assert!(!fixture.owner.has_outstanding_children(&parent()));
    assert!(
        fixture
            .owner
            .list_for_parent(&parent())
            .iter()
            .all(|child| child.activity == SubagentActivity::Idle)
    );
    fixture.tool.end_session(&parent()).await.expect("close");
}

#[tokio::test]
async fn a_user_continued_child_wakes_the_parent_like_a_background_child() {
    let fixture = fixture(SubagentLimits::default());
    let finished = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:1","agent":"explore","background":false}),
        )
        .await
        .expect("foreground child");
    assert_eq!(fixture.sink.wakes.load(Ordering::SeqCst), 0);
    let id = SubagentId(finished.data["id"].as_str().expect("id").to_owned());
    let ticket = fixture
        .owner
        .continue_child(
            &parent(),
            &id,
            "delay:1".to_owned(),
            fixture.owner.background_observer(fixture.sink.clone()),
            CancellationToken::default(),
        )
        .await
        .expect("continue");
    fixture
        .owner
        .wait(&ticket.handle)
        .await
        .expect("continued child completes");
    assert_eq!(
        fixture.sink.wakes.load(Ordering::SeqCst),
        1,
        "a continuation outside a tool call wakes an idle parent"
    );
    fixture.tool.end_session(&parent()).await.expect("close");
}

#[tokio::test]
async fn a_disabled_wake_also_covers_user_continued_children() {
    let fixture = fixture(SubagentLimits {
        wake_on_completion: false,
        ..SubagentLimits::default()
    });
    let finished = fixture
        .tool
        .execute(
            &fixture.context,
            json!({"action":"spawn","task":"delay:1","agent":"explore","background":false}),
        )
        .await
        .expect("foreground child");
    let id = SubagentId(finished.data["id"].as_str().expect("id").to_owned());
    let ticket = fixture
        .owner
        .continue_child(
            &parent(),
            &id,
            "delay:1".to_owned(),
            fixture.owner.background_observer(fixture.sink.clone()),
            CancellationToken::default(),
        )
        .await
        .expect("continue");
    fixture.owner.wait(&ticket.handle).await.expect("completes");
    assert_eq!(fixture.sink.wakes.load(Ordering::SeqCst), 0);
    fixture.tool.end_session(&parent()).await.expect("close");
}
