use std::sync::Arc;

use super::{FixtureProvider, collect, request, unique_temp_directory};
use crate::{FixtureRedactor, ProviderErrorKind, Recorder, ReplayProvider};

async fn recording() -> (std::path::PathBuf, std::path::PathBuf, serde_json::Value) {
    let directory = unique_temp_directory("schema-contract");
    let recorder = Recorder::new(
        Arc::new(FixtureProvider {
            name: "schema-fixture".to_owned(),
        }),
        &directory,
        FixtureRedactor::default(),
    );
    assert!(collect(&recorder).await.iter().all(Result::is_ok));
    recorder.flush().await.expect("fixture writer settles");
    let hash = super::request_hash(&request()).expect("request hash");
    let path = super::fixture_path(&directory, "schema-fixture", &hash, 0);
    let value = serde_json::from_slice(&tokio::fs::read(&path).await.expect("fixture read"))
        .expect("fixture JSON");
    (directory, path, value)
}

#[tokio::test]
async fn recording_rejects_missing_fields_in_each_required_object() {
    let (directory, path, fixture) = recording().await;
    for object in ["", "capabilities", "request"] {
        let value = if object.is_empty() {
            &fixture
        } else {
            &fixture[object]
        };
        for field in value.as_object().expect("schema object").keys() {
            let mut changed = fixture.clone();
            let target = if object.is_empty() {
                &mut changed
            } else {
                &mut changed[object]
            };
            target.as_object_mut().expect("schema object").remove(field);
            tokio::fs::write(&path, serde_json::to_vec(&changed).expect("fixture bytes"))
                .await
                .expect("fixture write");
            let error = ReplayProvider::load("schema-fixture", &directory)
                .await
                .expect_err("incomplete fixture must be rejected");
            assert_eq!(error.kind, ProviderErrorKind::Protocol, "{object}.{field}");
        }
    }
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("fixture cleanup");
}

#[tokio::test]
async fn recording_rejects_undeclared_fields_and_schema_identifiers() {
    let (directory, path, fixture) = recording().await;
    for mutation in ["schema", "field"] {
        let mut changed = fixture.clone();
        if mutation == "schema" {
            changed["version"] = serde_json::json!(u16::MAX);
        } else {
            changed["undeclared"] = serde_json::json!(true);
        }
        tokio::fs::write(&path, serde_json::to_vec(&changed).expect("fixture bytes"))
            .await
            .expect("fixture write");
        let error = ReplayProvider::load("schema-fixture", &directory)
            .await
            .expect_err("invalid fixture must be rejected");
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
    }
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("fixture cleanup");
}

#[tokio::test]
async fn copilot_stream_recording_requires_an_explicit_dialect() {
    let (directory, path, mut fixture) = recording().await;
    fixture["wire_mode"] = serde_json::json!("git_hub_copilot");
    fixture["capabilities"]["wire_mode"] = serde_json::json!("git_hub_copilot");
    fixture["raw_sse"] = serde_json::json!([{ "event": null, "data": "{}" }]);
    tokio::fs::write(&path, serde_json::to_vec(&fixture).expect("fixture bytes"))
        .await
        .expect("fixture write");
    let error = ReplayProvider::load("schema-fixture", &directory)
        .await
        .expect_err("stream without a dialect must be rejected");
    assert_eq!(error.kind, ProviderErrorKind::Protocol);
    assert!(error.message.contains("explicit stream dialect"));
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("fixture cleanup");
}

#[tokio::test]
async fn capability_manifest_requires_its_complete_schema() {
    let (directory, _, _) = recording().await;
    let path = super::super::capability_manifest_path(&directory, "schema-fixture");
    let manifest: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.expect("manifest read"))
            .expect("manifest JSON");
    for field in manifest.as_object().expect("manifest object").keys() {
        let mut changed = manifest.clone();
        changed
            .as_object_mut()
            .expect("manifest object")
            .remove(field);
        tokio::fs::write(&path, serde_json::to_vec(&changed).expect("manifest bytes"))
            .await
            .expect("manifest write");
        let error = ReplayProvider::load("schema-fixture", &directory)
            .await
            .expect_err("incomplete manifest must be rejected");
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
    }
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("fixture cleanup");
}
