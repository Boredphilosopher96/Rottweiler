#![allow(clippy::expect_used)]
use super::{FixtureProvider, fixture_path, request, request_hash, unique_temp_directory};
use crate::{FixtureRedactor, Provider as _, ProviderErrorKind, Recorder, ReplayProvider};
use futures_util::StreamExt as _;
use rw_types::{Block, Role, Turn, TurnMeta};
use std::sync::Arc;

#[tokio::test]
async fn recording_loader_requires_nullable_fields_in_canonical_turns() {
    let directory = unique_temp_directory("required-turn-fields");
    let mut request = request();
    request.turns.push(Turn {
        role: Role::Assistant,
        blocks: vec![
            Block::Thinking {
                content: "reasoning".into(),
                signature: None,
            },
            Block::Citation {
                uri: "https://example.org/source".into(),
                title: None,
                excerpt: None,
            },
        ],
        meta: TurnMeta::default(),
    });
    let recorder = Recorder::new(
        Arc::new(FixtureProvider {
            name: "turn-schema".into(),
        }),
        &directory,
        FixtureRedactor::default(),
    );
    let events = recorder
        .stream(request.clone())
        .await
        .expect("recording stream")
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    recorder.flush().await.expect("durable recording");
    let path = fixture_path(
        &directory,
        "turn-schema",
        &request_hash(&request).expect("hash"),
        0,
    );
    let bytes = tokio::fs::read(&path).await.expect("producer fixture");
    let complete: serde_json::Value = serde_json::from_slice(&bytes).expect("fixture JSON");
    ReplayProvider::load("turn-schema", &directory)
        .await
        .expect("explicit nulls are valid");
    for (pointer, field) in [
        ("/request/turns/0/meta", "created_at"),
        ("/request/turns/0/meta", "model"),
        ("/request/turns/0/blocks/0", "signature"),
        ("/request/turns/0/blocks/1", "title"),
        ("/request/turns/0/blocks/1", "excerpt"),
    ] {
        let mut missing = complete.clone();
        let removed = missing
            .pointer_mut(pointer)
            .and_then(serde_json::Value::as_object_mut)
            .expect("IR object")
            .remove(field);
        assert_eq!(removed, Some(serde_json::Value::Null));
        tokio::fs::write(
            &path,
            serde_json::to_vec(&missing).expect("modified fixture"),
        )
        .await
        .expect("persist one omission");
        let Err(error) = ReplayProvider::load("turn-schema", &directory).await else {
            panic!("accepted missing {pointer}/{field}");
        };
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
        assert!(
            error.message.contains(&format!("missing field `{field}`")),
            "{error:?}"
        );
    }
    tokio::fs::write(&path, bytes)
        .await
        .expect("restore producer bytes");
    ReplayProvider::load("turn-schema", &directory)
        .await
        .expect("restored recording");
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("fixture cleanup");
}
