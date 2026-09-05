#![cfg(test)]
use super::Arc;
use super::CancellationToken;
use super::FixtureRedactor;
use super::FixtureWebSearcher;
use super::Path;
use super::Read;
use super::RecordingConfiguredWebSearcher;
use super::ReplayingConfiguredWebSearcher;
use super::SequencedWebSearcher;
use super::WEBSEARCH_REPLAY_FILE;
use super::WebSearchFixtureDirectory;
use super::WebSearchRequest;
use super::WebSearchResponse;
use super::WebSearchResult;
use super::WebSearchSource;
use super::WebSearcher;
use super::deny_outbound_network_for_process;
use super::tempdir;

#[tokio::test]
async fn configured_websearch_records_redacted_and_replays_without_backend() {
    let fixtures = tempdir().expect("fixtures");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(fixtures.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private fixtures");
    }
    let redactor = FixtureRedactor::default();
    redactor.register_known_value("websearch-secret-canary");
    let inner: Arc<dyn WebSearcher> = Arc::new(FixtureWebSearcher(WebSearchResponse {
        source: WebSearchSource::ConfiguredApi,
        results: vec![WebSearchResult {
            title: "result websearch-secret-canary".to_owned(),
            url: "https://example.com/source".to_owned(),
            snippet: "snippet websearch-secret-canary".to_owned(),
        }],
    }));
    let writer =
        RecordingConfiguredWebSearcher::new(inner, fixtures.path(), redactor).expect("recorder");
    let request = WebSearchRequest {
        model_alias: Some("first-model".to_owned()),
        query: "fixture query".to_owned(),
        max_results: 5,
        recency_days: Some(7),
        allowed_domains: vec!["example.com".to_owned()],
    };
    let expected = writer
        .search(request.clone(), CancellationToken::default())
        .await
        .expect("recorded search");
    assert!(expected.results[0].snippet.contains("[REDACTED]"));
    let fixture_bytes =
        std::fs::read(fixtures.path().join(WEBSEARCH_REPLAY_FILE)).expect("fixture bytes");
    assert!(!String::from_utf8_lossy(&fixture_bytes).contains("websearch-secret-canary"));

    let replay = ReplayingConfiguredWebSearcher::load(fixtures.path())
        .expect("load replay")
        .expect("replay fixture");
    let mut switched_request = request;
    switched_request.model_alias = Some("switched-model".to_owned());
    let replayed = replay
        .search(switched_request, CancellationToken::default())
        .await
        .expect("replayed search");
    assert_eq!(replayed, expected);

    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("session_runtime::tests::configured_websearch_replay_network_denied_helper")
        .arg("--nocapture")
        .env("ROTTWEILER_WEBSEARCH_REPLAY_FIXTURE", fixtures.path())
        .status()
        .expect("network-denied replay subprocess");
    assert!(status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn configured_websearch_recording_ignores_planted_temporary_symlink() {
    use std::os::unix::fs::symlink;

    let fixtures = tempdir().expect("fixtures");
    let outside = tempdir().expect("outside");
    let canary = outside.path().join("canary");
    std::fs::write(&canary, b"must-not-change").expect("canary");
    symlink(&canary, fixtures.path().join("websearch.json.tmp"))
        .expect("planted temporary symlink");
    let writer = RecordingConfiguredWebSearcher::new(
        Arc::new(FixtureWebSearcher(WebSearchResponse {
            source: WebSearchSource::ConfiguredApi,
            results: Vec::new(),
        })),
        fixtures.path(),
        FixtureRedactor::default(),
    )
    .expect("secure recorder");
    writer
        .search(
            WebSearchRequest {
                model_alias: Some("fixture".to_owned()),
                query: "safe write".to_owned(),
                max_results: 1,
                recency_days: None,
                allowed_domains: Vec::new(),
            },
            CancellationToken::default(),
        )
        .await
        .expect("record search");
    assert_eq!(
        std::fs::read(&canary).expect("read canary"),
        b"must-not-change"
    );
    assert!(
        std::fs::symlink_metadata(fixtures.path().join("websearch.json.tmp"))
            .expect("planted symlink remains")
            .file_type()
            .is_symlink()
    );
    ReplayingConfiguredWebSearcher::load(fixtures.path())
        .expect("secure fixture loads")
        .expect("fixture exists");
}

#[cfg(unix)]
#[test]
fn configured_websearch_load_rejects_symlinks_and_reads_a_pinned_descriptor() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let fixtures = tempdir().expect("fixtures");
    let fixture_path = fixtures.path().join(WEBSEARCH_REPLAY_FILE);
    let original = br#"{"fixture":[]}"#;
    std::fs::write(&fixture_path, original).expect("fixture");
    std::fs::set_permissions(&fixture_path, std::fs::Permissions::from_mode(0o600))
        .expect("private fixture");
    let directory =
        WebSearchFixtureDirectory::open(fixtures.path(), false).expect("pinned fixture directory");
    let mut pinned = directory
        .open_fixture()
        .expect("open fixture")
        .expect("fixture exists");

    let moved = fixtures.path().join("moved.json");
    std::fs::rename(&fixture_path, &moved).expect("swap old path");
    let outside = tempdir().expect("outside");
    let replacement = outside.path().join("replacement.json");
    std::fs::write(&replacement, br#"{"attacker":[]}"#).expect("replacement");
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600))
        .expect("private replacement");
    symlink(&replacement, &fixture_path).expect("swapped symlink");

    let mut bytes = Vec::new();
    pinned.read_to_end(&mut bytes).expect("read pinned file");
    assert_eq!(bytes, original);
    assert!(ReplayingConfiguredWebSearcher::load(fixtures.path()).is_err());
}

#[cfg(unix)]
#[test]
fn configured_websearch_fixture_directory_rejects_symlink_and_unsafe_permissions() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let real = tempdir().expect("real directory");
    let parent = tempdir().expect("parent");
    let linked = parent.path().join("linked");
    symlink(real.path(), &linked).expect("directory symlink");
    assert!(WebSearchFixtureDirectory::open(&linked, false).is_err());

    std::fs::set_permissions(real.path(), std::fs::Permissions::from_mode(0o777))
        .expect("unsafe mode");
    assert!(WebSearchFixtureDirectory::open(real.path(), false).is_err());
}

#[tokio::test]
async fn configured_websearch_replay_network_denied_helper() {
    let Some(directory) = std::env::var_os("ROTTWEILER_WEBSEARCH_REPLAY_FIXTURE") else {
        return;
    };
    let _network_denial = deny_outbound_network_for_process();
    let replay = ReplayingConfiguredWebSearcher::load(Path::new(&directory))
        .expect("load replay")
        .expect("replay fixture");
    let response = replay
        .search(
            WebSearchRequest {
                model_alias: Some("network-denied".to_owned()),
                query: "fixture query".to_owned(),
                max_results: 5,
                recency_days: Some(7),
                allowed_domains: vec!["example.com".to_owned()],
            },
            CancellationToken::default(),
        )
        .await
        .expect("network-denied configured replay");
    assert_eq!(response.source, WebSearchSource::ConfiguredApi);
    assert_eq!(response.results.len(), 1);
}

#[tokio::test]
async fn configured_websearch_replay_preserves_repeated_request_occurrences() {
    let fixtures = tempdir().expect("fixtures");
    let writer = RecordingConfiguredWebSearcher::new(
        Arc::new(SequencedWebSearcher(std::sync::atomic::AtomicUsize::new(0))),
        fixtures.path(),
        FixtureRedactor::default(),
    )
    .expect("recorder");
    let request = WebSearchRequest {
        model_alias: Some("fixture".to_owned()),
        query: "repeated query".to_owned(),
        max_results: 5,
        recency_days: None,
        allowed_domains: Vec::new(),
    };
    for _ in 0..2 {
        writer
            .search(request.clone(), CancellationToken::default())
            .await
            .expect("record occurrence");
    }
    let replay = ReplayingConfiguredWebSearcher::load(fixtures.path())
        .expect("load replay")
        .expect("replay fixture");
    for expected in ["response-0", "response-1"] {
        let response = replay
            .search(request.clone(), CancellationToken::default())
            .await
            .expect("replay occurrence");
        assert_eq!(response.results[0].title, expected);
    }
    assert!(
        replay
            .search(request, CancellationToken::default())
            .await
            .is_err()
    );
}
