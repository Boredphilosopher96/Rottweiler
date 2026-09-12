use super::*;

#[tokio::test]
async fn search_excludes_other_workspaces_but_rejects_malformed_session_metadata() {
    let root = tempdir().expect("root");
    let allowed = private_test_directory(&root.path().join("allowed"));
    let outside = private_test_directory(&root.path().join("outside"));
    let writer =
        factory_with_allowed_workspaces(root.path(), vec![allowed.clone(), outside.clone()]).await;
    for (id, workspace) in [("search-allowed", &allowed), ("search-outside", &outside)] {
        let hosted = writer
            .create(CreateSessionRequest {
                session_id: SessionId(id.into()),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("seed exact metadata");
        hosted
            .handle()
            .snapshot()
            .await
            .expect("initialized durable source");
        hosted.handle().close().await.expect("close seed actor");
    }
    let index = SessionIndex::open(&writer.options.storage_root).expect("index");
    for (id, time) in [("search-allowed", 1), ("search-outside", 2)] {
        let mut projection = index
            .projection(id)
            .expect("read projection")
            .expect("source");
        projection.summary.updated_unix_ms = time;
        index
            .upsert(&projection)
            .expect("deterministic candidate order");
    }
    let reader = factory(root.path(), &allowed).await;
    let (hits, truncated) = reader
        .search_persisted_sessions("New session", 1)
        .await
        .expect("authorized search remains available");
    assert!(!truncated);
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].session.session_id,
        SessionId("search-allowed".into())
    );
    let metadata = reader
        .options
        .storage_root
        .join("sessions/search-outside/metadata.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata).expect("producer metadata")).expect("json");
    value
        .as_object_mut()
        .expect("metadata")
        .remove("workspace_generation");
    fs::write(
        metadata,
        serde_json::to_vec(&value).expect("single invalid mutation"),
    )
    .expect("invalid source");
    assert!(matches!(
        reader.search_persisted_sessions("New session", 1).await,
        Err(HostError::Persistence(_))
    ));
}
