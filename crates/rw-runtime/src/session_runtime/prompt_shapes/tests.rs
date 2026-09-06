#![allow(clippy::expect_used)]
use super::*;
use std::sync::Arc;

fn request() -> ProviderRequest {
    ProviderRequest {
        model: "model".into(),
        turns: vec![],
        tools: vec![],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 128,
        temperature: None,
        thinking: rw_types::config::ThinkingLevel::Off,
        cache_hint: None,
    }
}

#[tokio::test]
async fn abandoned_recording_keeps_its_worker_owned_until_commit_settles() {
    let root = tempfile::tempdir().expect("root");
    let session = root.path().join("sessions/session");
    std::fs::create_dir_all(&session).expect("session directory");
    let journal = Arc::new(PromptShapeJournal::open(root.path(), "session").expect("journal"));
    journal.set_active_turn(rw_core::TurnId("1".into()));
    journal.set_prompt_source(&rw_core::TurnId("1".into()), rw_types::SequenceId(9));
    let (entered, wait) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let locked = Arc::clone(&journal);
    let holder = std::thread::spawn(move || {
        let _store = locked.store.lock().expect("store gate");
        entered.send(()).expect("gate started");
        gate.recv().expect("release");
    });
    wait.await.expect("store locked");
    let worker = Arc::clone(&journal);
    let caller = tokio::spawn(async move {
        worker
            .record_owned("model".into(), request(), CacheBreakpointSupport::None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while journal.records.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("record admitted");
    caller.abort();
    assert!(caller.await.expect_err("abandoned caller").is_cancelled());
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            journal.settle_records()
        )
        .await
        .is_err()
    );
    release.send(()).expect("release actual worker");
    holder.join().expect("gate retired");
    tokio::time::timeout(std::time::Duration::from_secs(2), journal.settle_records())
        .await
        .expect("bounded settlement")
        .expect("record settled");
    assert_eq!(
        journal
            .shape_at_source(1, 9)
            .expect("committed source")
            .expect("shape")
            .1
            .source,
        9
    );
}

#[test]
fn profile_decoding_requires_the_complete_contract_and_admits_structure_first() {
    let profile = PromptShapeProfile {
        model_alias: "model".into(),
        tools: vec![],
        cache_support: CacheBreakpointSupport::None,
        cache_hint: None,
        cache_breakpoints: vec![],
    };
    let complete = serde_json::to_value(profile).expect("profile");
    for field in [
        "model_alias",
        "tools",
        "cache_support",
        "cache_hint",
        "cache_breakpoints",
    ] {
        let mut missing = complete.clone();
        missing.as_object_mut().expect("object").remove(field);
        assert!(
            serde_json::from_value::<PromptShapeProfile>(missing).is_err(),
            "required {field}"
        );
    }
    assert!(serde_json::from_value::<PromptCacheBreakpoint>(serde_json::json!({})).is_err());
    let deeply_nested = format!("{}0{}", "[".repeat(33), "]".repeat(33));
    assert!(admit_profile(deeply_nested.as_bytes()).is_err());
}

#[test]
fn encoded_profile_writer_bounds_capacity_across_large_incremental_writes() {
    use std::io::Write as _;
    let limit = rw_store::prompt_shapes::MAX_PROFILE_BYTES;
    let mut bytes = Vec::new();
    {
        let mut writer = rw_types::json_encoding::JsonWriter::buffer(&mut bytes, limit, 0)
            .expect("bounded profile");
        writer
            .write_all(&vec![b'a'; limit * 3 / 4])
            .expect("first chunk");
        writer
            .write_all(&vec![b'b'; limit / 4])
            .expect("second chunk");
        assert_eq!(writer.written(), limit);
    }
    assert!(bytes.capacity() <= limit);
    assert!(
        rw_types::json_encoding::JsonWriter::buffer(&mut bytes, limit, 0)
            .expect("bounded append")
            .write_all(b"overflow")
            .is_err()
    );
    assert_eq!(bytes.len(), limit);
    assert!(bytes.capacity() <= limit);
}
