#![allow(clippy::expect_used)]
use super::*;
use rw_types::{
    Cost, SessionId, SubagentId, Usage,
    workflow::{WorkflowRunId, WorkflowStepArtifact},
};
use std::{collections::BTreeMap, sync::Arc};

fn initial() -> WorkflowRunState {
    WorkflowRunState {
        run_id: WorkflowRunId::parse("a".repeat(32)).expect("run"),
        parent_session_id: SessionId("parent".to_owned()),
        workflow: "delivery".to_owned(),
        definition_digest: "b".repeat(64),
        tasks: ["plan", "build", "review"]
            .into_iter()
            .map(|id| (id.to_owned(), WorkflowTaskState::Pending))
            .collect::<BTreeMap<_, _>>(),
    }
}
fn task(state: &WorkflowRunState, name: &str) -> TaskId {
    TaskId {
        run_id: state.run_id.clone(),
        step_id: name.to_owned(),
    }
}
fn child() -> WorkflowChild {
    WorkflowChild {
        subagent_id: SubagentId("agent".to_owned()),
        session_id: SessionId("child".to_owned()),
    }
}
fn completed() -> WorkflowTaskOutcome {
    WorkflowTaskOutcome::Completed {
        artifact: Arc::new(WorkflowStepArtifact {
            subagent_id: child().subagent_id,
            child_session_id: child().session_id,
            final_text: "planned".to_owned(),
            touched_files: Vec::new(),
            diff_artifact: None,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Unavailable {
                reason: "fixture".to_owned(),
            },
        }),
    }
}

#[test]
fn reopening_preserves_receipts_and_ambiguous_claims_without_reexecution() {
    let root = tempfile::tempdir().expect("root");
    let expected = initial();
    let plan = task(&expected, "plan");
    let build = task(&expected, "build");
    let mut writer = WorkflowRunStore::open(root.path(), expected.clone()).expect("writer");
    writer
        .claim(std::slice::from_ref(&plan))
        .expect("claim plan");
    assert!(matches!(
        writer.settle(&plan, completed()),
        Err(WorkflowStoreError::Identity)
    ));
    writer.bind_child(&plan, child()).expect("child");
    writer.settle(&plan, completed()).expect("receipt");
    writer
        .claim(std::slice::from_ref(&build))
        .expect("claim build");
    drop(writer);
    let mut reopened = WorkflowRunStore::open(root.path(), expected).expect("reopen");
    assert!(matches!(
        reopened.state().tasks["plan"],
        WorkflowTaskState::Settled { .. }
    ));
    assert!(matches!(
        reopened.state().tasks["build"],
        WorkflowTaskState::Started { .. }
    ));
    assert!(matches!(
        reopened.claim(std::slice::from_ref(&plan)),
        Err(WorkflowStoreError::Transition)
    ));
    assert!(matches!(
        reopened.claim(&[build]),
        Err(WorkflowStoreError::Transition)
    ));
    reopened
        .settle(&plan, completed())
        .expect("exact receipt retry");
    assert!(matches!(
        reopened.settle(
            &plan,
            WorkflowTaskOutcome::Failed {
                message: "changed".to_owned()
            }
        ),
        Err(WorkflowStoreError::Transition)
    ));
}

#[test]
fn concurrent_writer_changed_definition_and_foreign_task_are_rejected() {
    let root = tempfile::tempdir().expect("root");
    let expected = initial();
    let mut writer = WorkflowRunStore::open(root.path(), expected.clone()).expect("writer");
    assert!(matches!(
        WorkflowRunStore::open(root.path(), expected.clone()),
        Err(WorkflowStoreError::Busy)
    ));
    let mut foreign = task(&expected, "plan");
    foreign.run_id = WorkflowRunId::parse("c".repeat(32)).expect("run");
    assert!(matches!(
        writer.claim(&[foreign]),
        Err(WorkflowStoreError::Identity)
    ));
    let plan = task(&expected, "plan");
    assert!(matches!(
        writer.claim(&[plan.clone(), plan]),
        Err(WorkflowStoreError::Transition)
    ));
    assert!(
        writer
            .state()
            .tasks
            .values()
            .all(|state| matches!(state, WorkflowTaskState::Pending))
    );
    drop(writer);
    let mut changed = expected.clone();
    changed.definition_digest = "d".repeat(64);
    assert!(matches!(
        WorkflowRunStore::open(root.path(), changed),
        Err(WorkflowStoreError::Identity)
    ));
    let mut changed = expected;
    changed.parent_session_id = SessionId("other-parent".to_owned());
    assert!(matches!(
        WorkflowRunStore::open(root.path(), changed),
        Err(WorkflowStoreError::Identity)
    ));
}

#[test]
fn oversized_outcomes_fail_before_replacing_the_started_obligation() {
    let root = tempfile::tempdir().expect("root");
    let expected = initial();
    let plan = task(&expected, "plan");
    let mut writer = WorkflowRunStore::open(root.path(), expected).expect("writer");
    writer.claim(std::slice::from_ref(&plan)).expect("claim");
    assert!(matches!(
        writer.settle(
            &plan,
            WorkflowTaskOutcome::Failed {
                message: "x".repeat(MAX_STATE_BYTES)
            }
        ),
        Err(WorkflowStoreError::Limit)
    ));
    assert!(matches!(
        writer.state().tasks["plan"],
        WorkflowTaskState::Started { .. }
    ));
}

#[test]
fn snapshots_are_available_during_execution_and_reject_other_parents() {
    let root = tempfile::tempdir().expect("root");
    let expected = initial();
    let mut writer = WorkflowRunStore::open(root.path(), expected.clone()).expect("writer");
    writer.claim(&[task(&expected, "plan")]).expect("claim");
    let state =
        WorkflowRunStore::snapshot(root.path(), &expected.run_id, &expected.parent_session_id)
            .expect("live snapshot");
    assert!(matches!(
        state.tasks["plan"],
        WorkflowTaskState::Started { .. }
    ));
    assert!(matches!(
        WorkflowRunStore::snapshot(
            root.path(),
            &expected.run_id,
            &SessionId("other".to_owned())
        ),
        Err(WorkflowStoreError::Identity)
    ));
    writer
        .bind_child(&task(&expected, "plan"), child())
        .expect("bind");
    writer
        .settle(&task(&expected, "plan"), completed())
        .expect("settle");
    assert!(matches!(
        state.tasks["plan"],
        WorkflowTaskState::Started { .. }
    ));
    let latest =
        WorkflowRunStore::snapshot(root.path(), &expected.run_id, &expected.parent_session_id)
            .expect("latest");
    assert!(matches!(
        latest.tasks["plan"],
        WorkflowTaskState::Settled { .. }
    ));
}

#[test]
fn process_exit_during_claim_preserves_obligation_and_releases_writer_lock() {
    const CHILD_ROOT: &str = "RW_WORKFLOW_CRASH_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let expected = initial();
        let mut writer = WorkflowRunStore::open(std::path::Path::new(&root), expected.clone())
            .expect("child writer");
        let plan = task(&expected, "plan");
        writer
            .claim(std::slice::from_ref(&plan))
            .expect("claim plan");
        writer.bind_child(&plan, child()).expect("child binding");
        writer
            .settle(&plan, completed())
            .expect("completed dependency");
        writer
            .claim(&[task(&expected, "build")])
            .expect("ambiguous build");
        // Exit bypasses all Rust destructors, including the writer owner.
        std::process::exit(77);
    }
    let root = tempfile::tempdir().expect("root");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "workflow::tests::process_exit_during_claim_preserves_obligation_and_releases_writer_lock", "--nocapture"])
        .env(CHILD_ROOT, root.path())
        .output().expect("child process");
    assert_eq!(
        output.status.code(),
        Some(77),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = initial();
    let mut reopened =
        WorkflowRunStore::open(root.path(), expected.clone()).expect("reopen after process exit");
    assert!(matches!(
        reopened.state().tasks["plan"],
        WorkflowTaskState::Settled { .. }
    ));
    assert!(matches!(
        reopened.state().tasks["build"],
        WorkflowTaskState::Started { .. }
    ));
    assert!(matches!(
        reopened.claim(&[task(&expected, "build")]),
        Err(WorkflowStoreError::Transition)
    ));
    assert!(matches!(
        reopened.state().tasks["review"],
        WorkflowTaskState::Pending
    ));
}
