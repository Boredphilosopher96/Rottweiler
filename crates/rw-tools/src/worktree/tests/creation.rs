use super::*;

#[tokio::test]
async fn rollback_removes_allocation_and_allows_another_child() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private root");
    let manager = isolation(repo.path(), private.path()).await;
    let allocation = manager
        .create(CancellationToken::default())
        .await
        .expect("allocate");
    let path = allocation.lease().path().to_path_buf();
    allocation.rollback().await.expect("rollback proof");
    assert!(!path.exists());
    assert!(
        !git(repo.path(), &["worktree", "list", "--porcelain"])
            .contains(path.to_str().expect("path"))
    );
    manager
        .create(CancellationToken::default())
        .await
        .expect("next allocation")
        .rollback()
        .await
        .expect("next rollback");
}

#[tokio::test]
async fn changed_allocation_is_preserved_and_blocks_further_creation() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private root");
    let manager = isolation(repo.path(), private.path()).await;
    let allocation = manager
        .create(CancellationToken::default())
        .await
        .expect("allocate");
    let path = allocation.lease().path().to_path_buf();
    std::fs::write(path.join("keep.txt"), "child effects").expect("write child output");
    assert!(matches!(
        allocation.rollback().await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert_eq!(
        std::fs::read_to_string(path.join("keep.txt")).expect("preserved"),
        "child effects"
    );
    let before = git(repo.path(), &["worktree", "list", "--porcelain"]);
    assert!(matches!(
        manager.clone().create(CancellationToken::default()).await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert_eq!(
        git(repo.path(), &["worktree", "list", "--porcelain"]),
        before
    );
}

#[tokio::test]
async fn abandoned_allocation_cannot_be_reported_as_clean() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private root");
    let manager = isolation(repo.path(), private.path()).await;
    let allocation = manager
        .create(CancellationToken::default())
        .await
        .expect("allocate");
    let path = allocation.lease().path().to_path_buf();
    drop(allocation);
    let error = manager
        .create(CancellationToken::default())
        .await
        .expect_err("blocked");
    assert!(matches!(error, ToolError::EffectsUnsettled(_)));
    assert!(error.to_string().contains(path.to_str().expect("path")));
    assert!(path.exists());
}

#[tokio::test]
async fn failed_cleanup_retains_the_registration_and_blocks_allocation() {
    let repo = repository();
    let private = tempfile::tempdir().expect("private root");
    let manager = isolation(repo.path(), private.path()).await;
    let allocation = manager
        .create(CancellationToken::default())
        .await
        .expect("allocate");
    let path = allocation.lease().path().to_path_buf();
    git(
        repo.path(),
        &["worktree", "lock", path.to_str().expect("path")],
    );
    let error = allocation.rollback().await.expect_err("locked cleanup");
    assert!(matches!(error, ToolError::EffectsUnsettled(_)));
    assert!(path.exists());
    assert!(matches!(
        manager.create(CancellationToken::default()).await,
        Err(ToolError::EffectsUnsettled(_))
    ));
}
