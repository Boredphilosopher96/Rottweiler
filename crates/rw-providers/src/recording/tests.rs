mod schema;

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use rw_types::config::ThinkingLevel;
use tokio::sync::Notify;

use crate::types::RawSseFrame;
use crate::{
    BoxEventStream, CacheBreakpointSupport, Capabilities, FinishReason, NativeWebSearchRequest,
    Provider, ProviderError, ProviderErrorKind, ProviderEvent, ProviderModelMetadata,
    ProviderRequest, TokenUsage, ToolChoice, UsageAccounting, WireFrameSink, WireMode,
};

use super::{FixtureRedactor, RecordFixture, Recorder, ReplayProvider, fixture_path, request_hash};

struct FixtureProvider {
    name: String,
}
struct ResponsesWithoutNativeProvider;

struct SequenceProvider {
    calls: AtomicUsize,
}

struct StartErrorProvider;

struct MetadataErrorProvider;

struct FlakyMetadataProvider {
    metadata_calls: AtomicUsize,
}

struct ResolvedMetadataStartErrorProvider;

struct RawPrefixProvider;

struct InterruptibleRawProvider;

struct RestrictedProvider;

struct DelayedStartProvider {
    calls: AtomicUsize,
    first_entered: Arc<Notify>,
    release_first: Arc<Notify>,
}

#[test]
fn shared_redactor_debug_is_safe_and_poisoning_retains_registered_secrets() {
    const FIRST: &str = "redactor-debug-canary-one";
    const SECOND: &str = "redactor-debug-canary-two";
    let redactor = FixtureRedactor::default();
    redactor.register_secret(&crate::Secret::new(FIRST));
    let lock = Arc::clone(&redactor.secrets);
    let _ = std::panic::catch_unwind(move || {
        let _guard = lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        panic!("deliberately poison redactor registry");
    });

    redactor.register_secret(&crate::Secret::new(SECOND));
    let rendered = redactor.redact(&format!("{FIRST} {SECOND}"));
    assert_eq!(rendered, "[REDACTED] [REDACTED]");
    let debug = format!("{redactor:?}");
    assert!(!debug.contains(FIRST));
    assert!(!debug.contains(SECOND));
    assert_eq!(redactor.registered_secret_count(), 2);
}

#[test]
fn byte_redaction_removes_registered_credentials_before_wire_encoding() {
    let redactor = FixtureRedactor::default();
    redactor.register_secret(&crate::Secret::new("binary-secret-canary"));
    let redacted = redactor.redact_bytes(b"prefix binary-secret-canary suffix");
    assert_eq!(redacted, b"prefix [REDACTED] suffix");
}

#[test]
fn strict_key_formats_are_redacted_without_corrupting_normal_code_data() {
    let redactor = FixtureRedactor::default();
    let private_key = "-----BEGIN OPENSSH PRIVATE KEY-----\ncanary-private-material\n-----END OPENSSH PRIVATE KEY-----";
    let values = [
        "sk-proj-abcdefghijklmnopqrstuvwxyz012345",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        "AKIAABCDEFGHIJKLMNOP",
        "AIzaabcdefghijklmnopqrstuvwxyz123456789",
        concat!("xoxb-", "1234567890-abcdefghijklmnopqrstuvwxyz"),
        "npm_abcdefghijklmnopqrstuvwxyz0123456789",
        private_key,
    ];
    let input = values.join("\n");
    let redacted = redactor.redact_text(&input);
    for value in values {
        assert!(!redacted.contains(value));
    }
    assert_eq!(redacted.matches("[REDACTED]").count(), 7);

    let ordinary = "sha256:0123456789abcdef0123456789abcdef and eyJub3QtYS10b2tlbiI6dHJ1ZX0";
    assert_eq!(redactor.redact_text(ordinary), ordinary);
}

#[async_trait]
impl Provider for SequenceProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "sequence"
    }

    fn capabilities(&self) -> Capabilities {
        test_capabilities()
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::TextDelta {
                text: format!("response-{call}"),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

#[async_trait]
impl Provider for StartErrorProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "start-error"
    }

    fn capabilities(&self) -> Capabilities {
        test_capabilities()
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Err(
            ProviderError::new(ProviderErrorKind::RateLimited, "fixture rate limit")
                .with_retry_after(1_250),
        )
    }
}

#[async_trait]
impl Provider for MetadataErrorProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "metadata-error"
    }

    fn capabilities(&self) -> Capabilities {
        test_capabilities()
    }

    async fn model_metadata(&self) -> Result<Option<crate::ProviderModelMetadata>, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::Server,
            "fixture metadata discovery failed",
        ))
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        panic!("stream must not run after metadata discovery failure")
    }
}

#[async_trait]
impl Provider for FlakyMetadataProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "flaky-metadata"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            wire_mode: WireMode::GitHubCopilot,
            ..test_capabilities()
        }
    }

    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        if self.metadata_calls.fetch_add(1, Ordering::Relaxed) == 0 {
            return Err(ProviderError::new(
                ProviderErrorKind::Server,
                "fixture transient metadata failure",
            ));
        }
        Ok(Some(flaky_metadata()))
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Ok(flaky_metadata_items(None))
    }

    async fn stream_with_wire_sink(
        &self,
        _request: ProviderRequest,
        sink: Arc<dyn WireFrameSink>,
    ) -> Result<BoxEventStream, ProviderError> {
        Ok(flaky_metadata_items(Some(&sink)))
    }
}

#[async_trait]
impl Provider for ResolvedMetadataStartErrorProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "resolved-metadata-start-error"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            wire_mode: WireMode::GitHubCopilot,
            ..test_capabilities()
        }
    }

    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        Ok(Some(flaky_metadata()))
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::Timeout,
            "fixture resolved provider start timeout",
        ))
    }
}

fn flaky_metadata() -> ProviderModelMetadata {
    ProviderModelMetadata {
        capabilities: Capabilities {
            tool_calling: true,
            vision: true,
            thinking: true,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: Some(200_000),
            max_output_tokens: Some(32_000),
            wire_mode: WireMode::GitHubCopilotResponses,
        },
        pricing: None,
        accounting: UsageAccounting::AiCredits {
            micros_usd_per_credit: 10_000,
        },
    }
}

fn flaky_metadata_items(sink: Option<&Arc<dyn WireFrameSink>>) -> BoxEventStream {
    let frames = [
        (
            "response.created",
            r#"{"response":{"model":"gpt-fixture"}}"#,
        ),
        ("response.output_text.delta", r#"{"delta":"resolved"}"#),
        ("response.completed", r#"{"response":{"usage":{}}}"#),
    ];
    if let Some(sink) = sink {
        for (event, data) in frames {
            sink.capture(Some(event), data);
        }
    }
    let raw = frames
        .into_iter()
        .map(|(event, data)| RawSseFrame {
            event: Some(event.to_owned()),
            data: data.to_owned(),
        })
        .collect::<Vec<_>>();
    Box::pin(futures_util::stream::iter(
        crate::github_copilot::replay_sse_frames(crate::GitHubCopilotEndpoint::Responses, &raw),
    ))
}

#[async_trait]
impl Provider for RawPrefixProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "raw-prefix"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            wire_mode: WireMode::AnthropicMessages,
            ..test_capabilities()
        }
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Ok(raw_prefix_items())
    }

    async fn stream_with_wire_sink(
        &self,
        _request: ProviderRequest,
        sink: Arc<dyn WireFrameSink>,
    ) -> Result<BoxEventStream, ProviderError> {
        sink.capture(
            Some("message_start"),
            r#"{"type":"message_start","message":{"model":"fixture-model","usage":{"input_tokens":3}}}"#,
        );
        Ok(raw_prefix_items())
    }
}

fn raw_prefix_items() -> BoxEventStream {
    Box::pin(futures_util::stream::iter([
        Ok(ProviderEvent::MessageStart {
            model: "fixture-model".to_owned(),
        }),
        Ok(ProviderEvent::Usage {
            usage: TokenUsage {
                input_tokens: 3,
                ..TokenUsage::default()
            },
        }),
        Err(ProviderError::new(
            ProviderErrorKind::Network,
            "fixture connection reset",
        )),
    ]))
}

#[async_trait]
impl Provider for InterruptibleRawProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "interruptible-raw"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            wire_mode: WireMode::AnthropicMessages,
            ..test_capabilities()
        }
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Ok(interruptible_raw_items(None))
    }

    async fn stream_with_wire_sink(
        &self,
        _request: ProviderRequest,
        sink: Arc<dyn WireFrameSink>,
    ) -> Result<BoxEventStream, ProviderError> {
        Ok(interruptible_raw_items(Some(sink)))
    }
}

#[async_trait]
impl Provider for RestrictedProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "restricted"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: false,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::Automatic,
            max_context_tokens: Some(2_048),
            max_output_tokens: Some(64),
            wire_mode: WireMode::NormalizedReplay,
        }
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::TextDelta {
                text: "restricted".to_owned(),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

fn interruptible_raw_items(sink: Option<Arc<dyn WireFrameSink>>) -> BoxEventStream {
    Box::pin(async_stream::stream! {
        if let Some(sink) = &sink {
            sink.capture(
                Some("content_block_delta"),
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"prefix"}}"#,
            );
        }
        yield Ok(ProviderEvent::TextDelta {
            text: "prefix".to_owned(),
        });
        if let Some(sink) = &sink {
            sink.capture(Some("message_stop"), r#"{"type":"message_stop"}"#);
        }
        yield Ok(ProviderEvent::Finished {
            reason: FinishReason::Stop,
        });
    })
}

#[async_trait]
impl Provider for DelayedStartProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "delayed"
    }

    fn capabilities(&self) -> Capabilities {
        test_capabilities()
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.first_entered.notify_one();
            self.release_first.notified().await;
        }
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::TextDelta {
                text: format!("response-{call}"),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

#[async_trait]
impl Provider for FixtureProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> Capabilities {
        test_capabilities()
    }
    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::TextDelta {
                text: "byte-identical".to_owned(),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

#[async_trait]
impl Provider for ResponsesWithoutNativeProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "responses-without-native"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            wire_mode: WireMode::OpenAiResponses,
            ..test_capabilities()
        }
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Ok(Box::pin(futures_util::stream::iter([Ok(
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        )])))
    }
}

pub(in crate::recording) fn request() -> ProviderRequest {
    ProviderRequest {
        model: "fixture-model".to_owned(),
        turns: Vec::new(),
        tools: Vec::new(),
        max_output_tokens: 10,
        temperature: None,
        thinking: ThinkingLevel::Off,
        tool_choice: ToolChoice::Auto {},
        cache_hint: None,
    }
}

fn test_capabilities() -> Capabilities {
    Capabilities {
        tool_calling: true,
        vision: true,
        thinking: true,
        cache_breakpoints: CacheBreakpointSupport::None,
        max_context_tokens: None,
        max_output_tokens: None,
        wire_mode: WireMode::NormalizedReplay,
    }
}

#[tokio::test]
async fn replay_is_byte_identical_and_does_not_call_live_provider() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("rw-provider-replay-{nonce}"));
    let recorder = Recorder::new(
        Arc::new(FixtureProvider {
            name: "fixture".to_owned(),
        }),
        &directory,
        FixtureRedactor::default(),
    );
    let live = recorder
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("record stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    let replay = ReplayProvider::load("fixture", &directory)
        .await
        .unwrap_or_else(|error| panic!("replay provider must load: {error}"))
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("replay must load: {error}"))
        .collect::<Vec<_>>()
        .await;
    let live_bytes =
        serde_json::to_vec(&live).unwrap_or_else(|error| panic!("live events serialize: {error}"));
    let replay_bytes = serde_json::to_vec(&replay)
        .unwrap_or_else(|error| panic!("replay events serialize: {error}"));
    assert_eq!(live_bytes, replay_bytes);
    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn responses_wire_mode_does_not_infer_native_search_support() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("rw-responses-no-search-{nonce}"));
    let recorder = Recorder::new(
        Arc::new(ResponsesWithoutNativeProvider),
        &directory,
        FixtureRedactor::default(),
    );
    recorder
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("record stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("recording must flush: {error}"));
    let replay = ReplayProvider::load("responses-without-native", &directory)
        .await
        .unwrap_or_else(|error| panic!("replay provider must load: {error}"));
    assert_eq!(
        replay.native_web_search_capability(),
        crate::NativeWebSearchCapability::Unsupported
    );
    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn native_search_marker_round_trips_through_normal_record_replay() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("rw-native-search-replay-{nonce}"));
    let mut search_request = request();
    search_request.tools = vec![
        NativeWebSearchRequest {
            query: "recorded search".to_owned(),
            max_results: 4,
            recency_days: None,
            allowed_domains: vec!["example.com".to_owned()],
        }
        .tool_definition()
        .unwrap_or_else(|error| panic!("search marker must encode: {error}")),
    ];
    let recorder = Recorder::new(
        Arc::new(FixtureProvider {
            name: "native-search".to_owned(),
        }),
        &directory,
        FixtureRedactor::default(),
    );
    let live = recorder
        .stream(search_request.clone())
        .await
        .unwrap_or_else(|error| panic!("record stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    let replay = ReplayProvider::load("native-search", &directory)
        .await
        .unwrap_or_else(|error| panic!("replay provider must load: {error}"))
        .stream(search_request)
        .await
        .unwrap_or_else(|error| panic!("replay must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(live, replay);
    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn repeated_requests_record_and_replay_distinct_occurrences_in_order() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("rw-provider-sequence-{nonce}"));
    let recorder = Recorder::new(
        Arc::new(SequenceProvider {
            calls: AtomicUsize::new(0),
        }),
        &directory,
        FixtureRedactor::default(),
    );

    let first_live = recorder
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("first record stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    let second_live = recorder
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("second record stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_ne!(first_live, second_live);

    let hash = request_hash(&request())
        .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
    for occurrence in 0..=1 {
        let path = fixture_path(&directory, "sequence", &hash, occurrence);
        let bytes = tokio::fs::read(&path)
            .await
            .unwrap_or_else(|error| panic!("fixture {} must exist: {error}", path.display()));
        let fixture: RecordFixture = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("fixture {} must deserialize: {error}", path.display()));
        assert_eq!(fixture.occurrence, occurrence);
    }

    let replay = ReplayProvider::load("sequence", &directory)
        .await
        .unwrap_or_else(|error| panic!("replay provider must load: {error}"));
    let first_replay = replay
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("first replay must load: {error}"))
        .collect::<Vec<_>>()
        .await;
    let second_replay = replay
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("second replay must load: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(first_live, first_replay);
    assert_eq!(second_live, second_replay);

    let Err(exhausted) = replay.stream(request()).await else {
        panic!("third replay must report occurrence exhaustion");
    };
    assert_eq!(exhausted.kind, ProviderErrorKind::ReplayMiss);
    assert!(exhausted.message.contains("exhausted at occurrence 2"));

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn interrupted_stream_records_partial_occurrence_without_leaving_a_hole() {
    let directory = unique_temp_directory("interrupted-occurrence");
    let recorder = Recorder::new(
        Arc::new(InterruptibleRawProvider),
        &directory,
        FixtureRedactor::default(),
    );
    let first_delta = ProviderEvent::TextDelta {
        text: "prefix".to_owned(),
    };

    let mut first_stream = recorder
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("first record stream must start: {error}"));
    assert_eq!(first_stream.next().await, Some(Ok(first_delta.clone())));
    drop(first_stream);
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("interrupted fixture must finalize: {error}"));

    let second_live = recorder
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("second record stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("second fixture must finalize: {error}"));

    let replay = ReplayProvider::load("interruptible-raw", &directory)
        .await
        .unwrap_or_else(|error| panic!("replay provider must load: {error}"));
    let first_replay = replay
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("first replay must load: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        first_replay,
        vec![
            Ok(first_delta),
            Err(ProviderError::new(
                ProviderErrorKind::Cancelled,
                "provider stream was interrupted before completion",
            )),
        ]
    );

    let second_replay = replay
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("second replay must load: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(second_replay, second_live);

    let hash = request_hash(&request())
        .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
    assert!(fixture_path(&directory, "interruptible-raw", &hash, 0).is_file());
    assert!(fixture_path(&directory, "interruptible-raw", &hash, 1).is_file());

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn replay_loads_exact_restricted_capabilities_and_rejects_inconsistency() {
    let directory = unique_temp_directory("restricted-capabilities");
    let live_capabilities = RestrictedProvider.capabilities();
    let recorder = Recorder::new(
        Arc::new(RestrictedProvider),
        &directory,
        FixtureRedactor::default(),
    );
    let _ = collect(&recorder).await;

    let replay = ReplayProvider::load("restricted", &directory)
        .await
        .unwrap_or_else(|error| panic!("restricted replay must load: {error}"));
    assert_eq!(replay.capabilities(), live_capabilities);

    let hash = request_hash(&request())
        .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
    let path = fixture_path(&directory, "restricted", &hash, 0);
    let bytes = tokio::fs::read(&path)
        .await
        .unwrap_or_else(|error| panic!("restricted fixture must read: {error}"));
    let mut fixture: RecordFixture = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("restricted fixture must parse: {error}"));
    fixture.capabilities.thinking = true;
    tokio::fs::write(
        &path,
        serde_json::to_vec_pretty(&fixture)
            .unwrap_or_else(|error| panic!("tampered fixture must serialize: {error}")),
    )
    .await
    .unwrap_or_else(|error| panic!("tampered fixture must write: {error}"));
    let error = ReplayProvider::load("restricted", &directory)
        .await
        .err()
        .unwrap_or_else(|| panic!("inconsistent capabilities must fail loading"));
    assert_eq!(error.kind, ProviderErrorKind::Protocol);
    assert!(error.message.contains("inconsistent"));

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn bounded_writer_backpressure_precedes_occurrence_assignment() {
    let directory = unique_temp_directory("writer-backpressure");
    let recorder = Arc::new(Recorder::with_writer_capacity(
        Arc::new(FixtureProvider {
            name: "bounded".to_owned(),
        }),
        &directory,
        FixtureRedactor::default(),
        1,
    ));
    let first_stream = recorder
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("first bounded stream must start: {error}"));
    let mut second_start = Box::pin(recorder.stream(request()));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second_start)
            .await
            .is_err(),
        "second stream must wait for bounded writer capacity"
    );
    drop(first_stream);
    let second_live = second_start
        .await
        .unwrap_or_else(|error| panic!("second bounded stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("bounded writer must flush: {error}"));

    let replay = ReplayProvider::load("bounded", &directory)
        .await
        .unwrap_or_else(|error| panic!("bounded replay must load: {error}"));
    let first_replay = replay
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("first bounded replay must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        first_replay.as_slice(),
        [Err(ProviderError {
            kind: ProviderErrorKind::Cancelled,
            ..
        })]
    ));
    let second_replay = replay
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("second bounded replay must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(second_replay, second_live);

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn writer_error_is_reported_by_stream_and_once_by_flush_barrier() {
    let root = unique_temp_directory("writer-error");
    tokio::fs::write(&root, b"not a directory")
        .await
        .unwrap_or_else(|error| panic!("writer-error sentinel must write: {error}"));
    let recorder = Recorder::new(
        Arc::new(FixtureProvider {
            name: "writer-error".to_owned(),
        }),
        &root,
        FixtureRedactor::default(),
    );
    let items = collect(&recorder).await;
    assert!(matches!(
        items.last(),
        Some(Err(ProviderError {
            kind: ProviderErrorKind::Protocol,
            ..
        }))
    ));
    assert!(recorder.settle_effects().await.is_ok());
    let first_error = recorder
        .flush()
        .await
        .err()
        .unwrap_or_else(|| panic!("first flush must surface the writer error"));
    assert_eq!(first_error.kind, ProviderErrorKind::Protocol);
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("second flush must clear prior error: {error}"));

    let _ = tokio::fs::remove_file(root).await;
}

#[tokio::test]
async fn providers_sharing_a_directory_do_not_collide() {
    let directory = unique_temp_directory("provider-scope");
    let alpha = Recorder::new(
        Arc::new(FixtureProvider {
            name: "alpha".to_owned(),
        }),
        &directory,
        FixtureRedactor::default(),
    );
    let beta = Recorder::new(
        Arc::new(FixtureProvider {
            name: "beta".to_owned(),
        }),
        &directory,
        FixtureRedactor::default(),
    );

    let alpha_live = collect(&alpha).await;
    let beta_live = collect(&beta).await;
    let hash = request_hash(&request())
        .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
    let alpha_path = fixture_path(&directory, "alpha", &hash, 0);
    let beta_path = fixture_path(&directory, "beta", &hash, 0);
    assert_ne!(alpha_path, beta_path);
    assert!(alpha_path.is_file());
    assert!(beta_path.is_file());

    let alpha_replay = collect(
        &ReplayProvider::load("alpha", &directory)
            .await
            .unwrap_or_else(|error| panic!("alpha replay must load: {error}")),
    )
    .await;
    let beta_replay = collect(
        &ReplayProvider::load("beta", &directory)
            .await
            .unwrap_or_else(|error| panic!("beta replay must load: {error}")),
    )
    .await;
    assert_eq!(alpha_live, alpha_replay);
    assert_eq!(beta_live, beta_replay);

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn replay_rejects_a_fixture_claiming_another_provider() {
    let directory = unique_temp_directory("provider-validation");
    let beta = Recorder::new(
        Arc::new(FixtureProvider {
            name: "beta".to_owned(),
        }),
        &directory,
        FixtureRedactor::default(),
    );
    let recorded = collect(&beta).await;
    assert!(recorded.iter().all(Result::is_ok));
    beta.flush()
        .await
        .unwrap_or_else(|error| panic!("recording settles: {error}"));
    ReplayProvider::load("beta", &directory)
        .await
        .unwrap_or_else(|error| panic!("complete source loads before mutation: {error}"));
    let hash = request_hash(&request())
        .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
    let path = fixture_path(&directory, "beta", &hash, 0);
    let bytes = tokio::fs::read(&path)
        .await
        .unwrap_or_else(|error| panic!("fixture read: {error}"));
    let mut fixture = super::catalog::decode_fixture(&bytes)
        .unwrap_or_else(|error| panic!("complete source decodes: {error}"));
    fixture.provider = "alpha".to_owned();
    tokio::fs::write(
        &path,
        super::catalog::encode_fixture(&fixture)
            .unwrap_or_else(|error| panic!("mutated source encodes: {error}")),
    )
    .await
    .unwrap_or_else(|error| panic!("fixture write: {error}"));
    let Err(error) = ReplayProvider::load("beta", &directory).await else {
        panic!("provider mismatch must be rejected");
    };
    assert_eq!(error.kind, ProviderErrorKind::Protocol);
    assert!(error.message.contains("provider"));
    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn provider_start_error_is_recorded_and_replayed_as_start_error() {
    let directory = unique_temp_directory("start-error");
    let recorder = Recorder::new(
        Arc::new(StartErrorProvider),
        &directory,
        FixtureRedactor::default(),
    );

    let Err(live_error) = recorder.stream(request()).await else {
        panic!("live provider start must fail");
    };
    let replay = ReplayProvider::load("start-error", &directory)
        .await
        .unwrap_or_else(|error| panic!("start-error replay must load: {error}"));
    let Err(replay_error) = replay.stream(request()).await else {
        panic!("replayed provider start must fail");
    };
    assert_eq!(live_error, replay_error);
    assert!(replay_error.is_retryable());
    assert_eq!(replay_error.retry_after_ms, Some(1_250));

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn metadata_discovery_error_is_recorded_without_an_occurrence_hole() {
    let directory = unique_temp_directory("metadata-start-error");
    let recorder = Recorder::new(
        Arc::new(MetadataErrorProvider),
        &directory,
        FixtureRedactor::default(),
    );

    let Err(live_error) = recorder.stream(request()).await else {
        panic!("metadata discovery must fail");
    };
    let replay = ReplayProvider::load("metadata-error", &directory)
        .await
        .unwrap_or_else(|error| panic!("metadata-error replay must load: {error}"));
    let Err(replay_error) = replay.stream(request()).await else {
        panic!("metadata discovery start error must replay");
    };
    assert_eq!(live_error, replay_error);
    assert_eq!(replay_error.kind, ProviderErrorKind::Server);
    let hash = request_hash(&request())
        .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
    assert!(fixture_path(&directory, "metadata-error", &hash, 0).is_file());
    assert!(!fixture_path(&directory, "metadata-error", &hash, 1).is_file());

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn metadata_discovery_failure_and_resolved_stream_replay_in_order() {
    let directory = unique_temp_directory("metadata-evolution");
    let recorder = Recorder::new(
        Arc::new(FlakyMetadataProvider {
            metadata_calls: AtomicUsize::new(0),
        }),
        &directory,
        FixtureRedactor::default(),
    );

    let Err(first_live) = recorder.stream(request()).await else {
        panic!("first metadata discovery must fail");
    };
    let second_live = collect(&recorder).await;
    assert!(matches!(
        second_live.as_slice(),
        [
            Ok(ProviderEvent::MessageStart { .. }),
            Ok(ProviderEvent::TextDelta { text }),
            Ok(ProviderEvent::Usage { .. }),
            Ok(ProviderEvent::Finished { .. })
        ] if text == "resolved"
    ));

    let replay = ReplayProvider::load("flaky-metadata", &directory)
        .await
        .unwrap_or_else(|error| panic!("evolved replay must load: {error}"));
    let metadata = replay
        .model_metadata()
        .await
        .unwrap_or_else(|error| panic!("replay metadata must resolve: {error}"))
        .unwrap_or_else(|| panic!("resolved metadata must persist"));
    assert_eq!(
        metadata.capabilities.wire_mode,
        WireMode::GitHubCopilotResponses
    );
    let Err(first_replay) = replay.stream(request()).await else {
        panic!("first replay occurrence must retain discovery failure");
    };
    assert_eq!(first_live, first_replay);
    let second_replay = collect(&replay).await;
    assert_eq!(second_live, second_replay);

    let hash = request_hash(&request())
        .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
    let first_bytes = tokio::fs::read(fixture_path(&directory, "flaky-metadata", &hash, 0))
        .await
        .unwrap_or_else(|error| panic!("first fixture must read: {error}"));
    let first: RecordFixture = serde_json::from_slice(&first_bytes)
        .unwrap_or_else(|error| panic!("first fixture must parse: {error}"));
    let second_bytes = tokio::fs::read(fixture_path(&directory, "flaky-metadata", &hash, 1))
        .await
        .unwrap_or_else(|error| panic!("second fixture must read: {error}"));
    let second: RecordFixture = serde_json::from_slice(&second_bytes)
        .unwrap_or_else(|error| panic!("second fixture must parse: {error}"));
    assert_eq!(first.model_metadata, None);
    assert_eq!(first.wire_mode, WireMode::GitHubCopilot);
    assert!(second.model_metadata.is_some());
    assert_eq!(second.wire_mode, WireMode::GitHubCopilotResponses);

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn resolved_exact_dialect_replays_empty_raw_start_error() {
    let directory = unique_temp_directory("resolved-empty-raw");
    let recorder = Recorder::new(
        Arc::new(ResolvedMetadataStartErrorProvider),
        &directory,
        FixtureRedactor::default(),
    );
    let Err(live_error) = recorder.stream(request()).await else {
        panic!("resolved provider start must fail");
    };
    let replay = ReplayProvider::load("resolved-metadata-start-error", &directory)
        .await
        .unwrap_or_else(|error| panic!("resolved replay must load: {error}"));
    assert_eq!(
        replay.capabilities().wire_mode,
        WireMode::GitHubCopilotResponses
    );
    let Err(replay_error) = replay.stream(request()).await else {
        panic!("empty-raw start error must replay");
    };
    assert_eq!(live_error, replay_error);

    let hash = request_hash(&request())
        .unwrap_or_else(|error| panic!("fixture request must hash: {error}"));
    let bytes = tokio::fs::read(fixture_path(
        &directory,
        "resolved-metadata-start-error",
        &hash,
        0,
    ))
    .await
    .unwrap_or_else(|error| panic!("fixture must read: {error}"));
    let fixture: RecordFixture = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("fixture must parse: {error}"));
    assert_eq!(fixture.wire_mode, WireMode::GitHubCopilotResponses);
    assert!(fixture.raw_sse.is_empty());

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn raw_frame_prefix_replays_the_recorded_transport_error() {
    let directory = unique_temp_directory("raw-prefix");
    let recorder = Recorder::new(
        Arc::new(RawPrefixProvider),
        &directory,
        FixtureRedactor::default(),
    );

    let live = collect(&recorder).await;
    let replay = collect(
        &ReplayProvider::load("raw-prefix", &directory)
            .await
            .unwrap_or_else(|error| panic!("raw-prefix replay must load: {error}")),
    )
    .await;
    assert_eq!(live, replay);
    assert!(matches!(
        replay.last(),
        Some(Err(ProviderError {
            kind: ProviderErrorKind::Network,
            message,
            ..
        })) if message == "fixture connection reset"
    ));

    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn concurrent_start_completion_order_does_not_renumber_occurrences() {
    let directory = unique_temp_directory("start-order");
    let first_entered = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let recorder = Arc::new(Recorder::new(
        Arc::new(DelayedStartProvider {
            calls: AtomicUsize::new(0),
            first_entered: Arc::clone(&first_entered),
            release_first: Arc::clone(&release_first),
        }),
        &directory,
        FixtureRedactor::default(),
    ));

    let first_task = tokio::spawn({
        let recorder = Arc::clone(&recorder);
        async move { collect(recorder.as_ref()).await }
    });
    first_entered.notified().await;
    let second_task = tokio::spawn({
        let recorder = Arc::clone(&recorder);
        async move { collect(recorder.as_ref()).await }
    });
    let second_live = second_task
        .await
        .unwrap_or_else(|error| panic!("second record task must join: {error}"));
    release_first.notify_one();
    let first_live = first_task
        .await
        .unwrap_or_else(|error| panic!("first record task must join: {error}"));
    assert!(matches!(
        first_live.first(),
        Some(Ok(ProviderEvent::TextDelta { text })) if text == "response-0"
    ));
    assert!(matches!(
        second_live.first(),
        Some(Ok(ProviderEvent::TextDelta { text })) if text == "response-1"
    ));

    let replay = ReplayProvider::load("delayed", &directory)
        .await
        .unwrap_or_else(|error| panic!("delayed replay must load: {error}"));
    let first_replay = collect(&replay).await;
    let second_replay = collect(&replay).await;
    assert_eq!(first_replay, first_live);
    assert_eq!(second_replay, second_live);

    let _ = tokio::fs::remove_dir_all(directory).await;
}

pub(in crate::recording) fn unique_temp_directory(label: &str) -> std::path::PathBuf {
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let process = std::process::id();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("rw-provider-{label}-{process}-{nonce}-{sequence}"))
}

async fn collect(provider: &dyn Provider) -> Vec<Result<ProviderEvent, ProviderError>> {
    provider
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("fixture stream must start: {error}"))
        .collect()
        .await
}
