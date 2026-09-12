#![allow(clippy::expect_used)]
use super::*;
use crate::{
    AuthMaterial, AuthProvider, CacheBreakpointSupport, NetworkPolicy, OpenAiChatRequestProfile,
    OpenAiCompatibleConfig, OpenAiCompatibleProvider, OpenAiWireMode, Provider, ProviderError,
    ProviderErrorKind, ProviderEvent,
};
use futures_util::StreamExt;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[derive(Debug, Default)]
struct CountAuth(AtomicUsize);
#[async_trait::async_trait]
impl AuthProvider for CountAuth {
    async fn material(&self) -> Result<AuthMaterial, ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(AuthMaterial::None)
    }
}
fn config(
    endpoint: url::Url,
    mode: OpenAiWireMode,
    auth: Arc<CountAuth>,
) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig {
        name: "structured-http".into(),
        endpoint,
        auth,
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Allow,
        wire_mode: mode,
        chat_request_profile: OpenAiChatRequestProfile::OpenAi,
        tool_calling: true,
        cache_breakpoints: CacheBreakpointSupport::None,
        supported_reasoning_efforts: vec![],
        supports_vision: false,
        max_context_tokens: None,
        max_output_tokens: None,
        headers: BTreeMap::new(),
        header_credentials: BTreeMap::new(),
        extra_body: BTreeMap::new(),
        model_ids: BTreeMap::new(),
        path_template: None,
    }
}
#[tokio::test]
async fn unsupported_or_incompatible_output_never_resolves_authentication() {
    let auth = Arc::new(CountAuth::default());
    let mut settings = config(
        "http://127.0.0.1:9/completions".parse().expect("URL"),
        OpenAiWireMode::ChatCompletions,
        auth.clone(),
    );
    settings.chat_request_profile = OpenAiChatRequestProfile::Compatible;
    let provider = OpenAiCompatibleProvider::new(settings).expect("provider");
    let Err(error) = provider.stream(super::tests::request()).await else {
        panic!("unsupported")
    };
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);
    let mut invalid = super::tests::request();
    invalid.tool_choice = crate::ToolChoice::Auto {};
    let Err(error) = provider.stream(invalid).await else {
        panic!("incompatible")
    };
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(auth.0.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn direct_openai_http_projects_and_validates_both_supported_wire_dialects() {
    for mode in [OpenAiWireMode::ChatCompletions, OpenAiWireMode::Responses] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let endpoint = format!(
            "http://{}/endpoint",
            listener.local_addr().expect("address")
        )
        .parse()
        .expect("URL");
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5),async move {
                let (mut connection,_)=listener.accept().await.expect("request");
                let request=read_request(&mut connection).await;
                let format=if mode==OpenAiWireMode::Responses {&request["text"]["format"]} else {&request["response_format"]["json_schema"]};
                assert_eq!(format["strict"],true);
                assert_eq!(format["schema"]["required"],serde_json::json!(["count","labels","optional"]));
                let text=r#"{"count":1,"labels":[],"optional":null}"#;
                let frames=match mode {
                    OpenAiWireMode::ChatCompletions=>vec![serde_json::json!({"model":"fixture","choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]}).to_string(),serde_json::json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":4}}).to_string(),"[DONE]".into()],
                    OpenAiWireMode::Responses=>vec![serde_json::json!({"type":"response.output_text.delta","delta":text}).to_string(),serde_json::json!({"type":"response.completed","response":{"usage":{}}}).to_string()],
                };
                let mut body = String::new();
                for frame in frames {
                    body.push_str("data: ");
                    body.push_str(&frame);
                    body.push_str("\n\n");
                }
                let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                connection.write_all(response.as_bytes()).await.expect("response");
            }).await.expect("server lifetime");
        });
        let auth = Arc::new(CountAuth::default());
        let provider =
            OpenAiCompatibleProvider::new(config(endpoint, mode, auth.clone())).expect("provider");
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            provider
                .stream(super::tests::request())
                .await
                .expect("stream")
                .collect::<Vec<_>>()
                .await
        })
        .await
        .expect("response lifetime");
        server.await.expect("server joined");
        assert!(observed.iter().all(Result::is_ok), "{observed:?}");
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event, Ok(ProviderEvent::Finished { .. })))
                .count(),
            1
        );
        assert!(observed.iter().any(|event|matches!(event,Ok(ProviderEvent::TextDelta{text}) if text.contains("\"count\":1"))));
        if mode == OpenAiWireMode::ChatCompletions {
            let usage = observed.iter().position(|event| matches!(event, Ok(ProviderEvent::Usage {usage}) if usage.output_tokens == 4)).expect("usage after choice stop");
            let finished = observed
                .iter()
                .position(|event| matches!(event, Ok(ProviderEvent::Finished { .. })))
                .expect("terminal");
            assert!(usage < finished);
        }
        assert_eq!(auth.0.load(Ordering::SeqCst), 1);
    }
}
async fn read_request(connection: &mut tokio::net::TcpStream) -> serde_json::Value {
    let mut bytes = Vec::with_capacity(8192);
    let (header, length) = loop {
        assert!(bytes.len() < 8192, "bounded fixture request");
        let mut chunk = [0; 1024];
        let read = connection.read(&mut chunk).await.expect("read");
        assert_ne!(read, 0);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(header) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let text = std::str::from_utf8(&bytes[..header]).expect("HTTP");
            let length = text
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().expect("length"))
                })
                .expect("content length");
            break (header + 4, length);
        }
    };
    assert!(header + length <= 8192);
    while bytes.len() < header + length {
        let mut chunk = [0; 1024];
        let read = connection.read(&mut chunk).await.expect("body");
        assert_ne!(read, 0);
        bytes.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&bytes[header..header + length]).expect("JSON body")
}
#[test]
fn refusals_are_recognized_even_when_refusal_text_itself_is_valid_json() {
    for value in [
        serde_json::json!({"choices":[{"delta":{"refusal":"{}"}}]}),
        serde_json::json!({"type":"response.refusal.delta","delta":"{}"}),
        serde_json::json!({"type":"response.content_part.added","part":{"type":"refusal","refusal":"{}"}}),
    ] {
        assert!(wire::is_refusal(&value));
    }
}

#[test]
fn direct_construction_cannot_override_structured_or_tool_contracts() {
    for (key, value) in [
        ("response_format", serde_json::json!({"type":"json_object"})),
        ("text", serde_json::json!({"format":{"type":"json_schema"}})),
        ("tools", serde_json::json!([])),
    ] {
        let mut settings = config(
            "http://127.0.0.1:9/".parse().expect("URL"),
            OpenAiWireMode::Responses,
            Arc::new(CountAuth::default()),
        );
        settings.extra_body.insert(key.into(), value);
        let Err(error) = OpenAiCompatibleProvider::new(settings) else {
            panic!("controlled field override")
        };
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    }
}
#[test]
fn actual_normalizers_reject_json_shaped_refusals_in_structured_mode() {
    for (mode, data) in [
        (
            OpenAiWireMode::ChatCompletions,
            serde_json::json!({"choices":[{"index":0,"delta":{"refusal":"{}"},"finish_reason":"stop"}]}),
        ),
        (
            OpenAiWireMode::Responses,
            serde_json::json!({"type":"response.refusal.delta","delta":"{}"}),
        ),
    ] {
        let frames = [crate::types::RawSseFrame {
            event: None,
            data: data.to_string(),
        }];
        let events = crate::openai::replay_sse_frames(mode, &frames, true);
        assert!(
            matches!(events.as_slice(),[Err(error)] if error.kind == ProviderErrorKind::Protocol && error.message == "structured output was refused")
        );
    }
}
