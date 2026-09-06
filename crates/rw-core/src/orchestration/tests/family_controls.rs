use super::{FakeFactory, SubagentLimits, orchestrator, recovery_record};
use rw_types::SessionId;
use std::sync::Arc;

#[tokio::test]
async fn family_discovery_binds_every_hop_and_rejects_foreign_roots() {
    let owner = orchestrator(SubagentLimits::default(), Arc::new(FakeFactory::default()));
    owner
        .recover_record(recovery_record("first", "first-session"))
        .await
        .expect("first");
    let mut nested = recovery_record("second", "second-session");
    nested.parent_session_id = SessionId("first-session".into());
    nested.depth = 2;
    owner.recover_record(nested).await.expect("second");
    let root = SessionId("parent".into());
    let snapshot = owner
        .family_controls(&root)
        .expect("discover without progress");
    assert_eq!(snapshot.children.len(), 2);
    let child = snapshot
        .children
        .iter()
        .find(|row| row.target.session_id.0 == "second-session")
        .expect("nested");
    assert_eq!(child.target.ancestry.len(), 2);
    assert_eq!(child.target.ancestry[0].session_id.0, "first-session");
    assert_eq!(
        owner
            .control_child(&root, &child.target)
            .expect("owned")
            .session_id()
            .0,
        "second-session"
    );
    assert!(
        owner
            .control_child(&SessionId("foreign".into()), &child.target)
            .is_err()
    );
    let mut wrong = child.target.clone();
    wrong.ancestry[0].session_id = SessionId("different-session".into());
    assert!(owner.control_child(&root, &wrong).is_err());
    assert!(
        owner
            .family_controls(&SessionId("foreign".into()))
            .expect("unrelated root")
            .children
            .is_empty()
    );
}
