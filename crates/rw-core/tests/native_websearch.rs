use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rw_core::ProviderNativeWebSearcher;
use rw_core::runtime_support::{
    BoxEventStream, CacheBreakpointSupport, CancellationToken, Capabilities, FinishReason,
    FixtureRedactor, NativeWebSearchCapability, Provider, ProviderError, ProviderEvent,
    ProviderRequest, Recorder, ReplayProvider, WebSearchRequest, WebSearchSource, WebSearcher,
    WireMode, deny_outbound_network_for_process,
};

struct NativeFixtureProvider {
    request: Mutex<Option<ProviderRequest>>,
}

fn fixture_request() -> WebSearchRequest {
    WebSearchRequest {
        model_alias: Some("fixture".to_owned()),
        query: "fixture query".to_owned(),
        max_results: 5,
        recency_days: None,
        allowed_domains: vec!["example.com".to_owned()],
    }
}

#[async_trait]
impl Provider for NativeFixtureProvider {
    fn name(&self) -> &'static str {
        "native-fixture"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::Automatic,
            max_context_tokens: Some(16_384),
            max_output_tokens: Some(2_048),
            wire_mode: WireMode::OpenAiResponses,
        }
    }

    fn native_web_search_capability(&self) -> NativeWebSearchCapability {
        NativeWebSearchCapability::Supported
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        *self
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::TextDelta {
                text: "bounded answer".to_owned(),
            }),
            Ok(ProviderEvent::Citation {
                uri: "https://example.com/source".to_owned(),
                title: Some("Example".to_owned()),
                start_index: Some(0),
                end_index: Some(7),
            }),
            Ok(ProviderEvent::Citation {
                uri: "https://example.com/source".to_owned(),
                title: Some("Duplicate".to_owned()),
                start_index: None,
                end_index: None,
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

#[tokio::test]
async fn provider_native_search_uses_normal_stream_and_deduplicates_citations()
-> Result<(), Box<dyn std::error::Error>> {
    let provider = Arc::new(NativeFixtureProvider {
        request: Mutex::new(None),
    });
    let searcher = ProviderNativeWebSearcher::new(provider.clone(), "gpt-fixture".to_owned())
        .ok_or_else(|| std::io::Error::other("fixture provider must support native search"))?;
    let response = searcher
        .search(fixture_request(), CancellationToken::default())
        .await?;
    assert_eq!(response.source, WebSearchSource::ProviderNative);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].title, "Example");
    assert_eq!(response.results[0].snippet, "bounded answer");
    let request = provider
        .request
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .ok_or_else(|| std::io::Error::other("provider did not capture native search request"))?;
    assert_eq!(request.model, "gpt-fixture");
    assert_eq!(request.tools.len(), 1);
    assert_eq!(
        request.tools[0].name,
        "__rottweiler_provider_native_web_search"
    );
    Ok(())
}

#[tokio::test]
async fn recorded_native_search_replays_with_process_network_denied()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let live: Arc<dyn Provider> = Arc::new(NativeFixtureProvider {
        request: Mutex::new(None),
    });
    let recorder = Arc::new(Recorder::new(
        live,
        directory.path(),
        FixtureRedactor::default(),
    ));
    let recording_provider: Arc<dyn Provider> = recorder.clone();
    let recording = ProviderNativeWebSearcher::new(recording_provider, "gpt-fixture".to_owned())
        .ok_or_else(|| std::io::Error::other("recorder must retain native capability"))?;
    let expected = recording
        .search(fixture_request(), CancellationToken::default())
        .await?;
    recorder.flush().await?;

    let replay: Arc<dyn Provider> =
        Arc::new(ReplayProvider::load("native-fixture", directory.path()).await?);
    let replaying = ProviderNativeWebSearcher::new(replay, "gpt-fixture".to_owned())
        .ok_or_else(|| std::io::Error::other("replay must advertise native capability"))?;
    let _network_denial = deny_outbound_network_for_process();
    let actual = replaying
        .search(fixture_request(), CancellationToken::default())
        .await?;
    assert_eq!(actual, expected);
    Ok(())
}
