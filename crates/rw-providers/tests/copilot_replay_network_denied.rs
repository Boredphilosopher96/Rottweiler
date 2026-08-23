use std::{path::PathBuf, sync::Arc};

use futures_util::StreamExt;
use rw_providers::{
    GitHubCopilotProvider, GitHubCopilotProviderConfig, GitHubCopilotRuntime, NetworkPolicy,
    Provider, ProviderRequest, Recorder, ReplayProvider, Secret, ToolChoice,
    deny_outbound_network_for_process,
};
use rw_types::{Block, Role, Turn, TurnMeta, config::ThinkingLevel};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;

const CATALOG: &str = r#"{"data":[{"model_picker_enabled":true,"id":"fixture-model","name":"Fixture Copilot","version":"fixture-model-2026-07-10","supported_endpoints":["/chat/completions"],"policy":{"state":"enabled"},"capabilities":{"family":"gpt","limits":{"max_context_window_tokens":100000,"max_output_tokens":4096,"max_prompt_tokens":90000},"supports":{"tool_calls":true,"reasoning_effort":["none"]}}}]}"#;

fn request() -> ProviderRequest {
    ProviderRequest {
        model: "fixture-model".to_owned(),
        turns: vec![Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "respond once".to_owned(),
            }],
            meta: TurnMeta::default(),
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        max_output_tokens: 32,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    }
}

fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn sse_response() -> String {
    let body = "data: {\"id\":\"chat-1\",\"model\":\"fixture-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"offline-ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn read_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        let read = socket
            .read(&mut chunk)
            .await
            .unwrap_or_else(|error| panic!("fixture request must read: {error}"));
        assert_ne!(read, 0, "fixture request closed before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let read = socket
            .read(&mut chunk)
            .await
            .unwrap_or_else(|error| panic!("fixture body must read: {error}"));
        assert_ne!(read, 0, "fixture request closed before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn spawn_origin() -> (Url, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("Copilot fixture listener must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("Copilot fixture address must resolve: {error}"));
    let origin = Url::parse(&format!("http://{address}/"))
        .unwrap_or_else(|error| panic!("Copilot fixture origin must parse: {error}"));
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in [json_response(CATALOG), sse_response()] {
            let (mut socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("Copilot fixture request must arrive: {error}"));
            requests.push(read_request(&mut socket).await);
            socket
                .write_all(response.as_bytes())
                .await
                .unwrap_or_else(|error| panic!("Copilot fixture response must write: {error}"));
        }
        requests
    });
    (origin, task)
}

fn fixture_directory() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rottweiler-copilot-network-denied-replay-{}",
        std::process::id()
    ))
}

#[tokio::test]
async fn copilot_replay_succeeds_under_process_wide_network_denial() {
    let directory = fixture_directory();
    if directory.exists() {
        std::fs::remove_dir_all(&directory)
            .unwrap_or_else(|error| panic!("stale fixture directory must remove: {error}"));
    }
    let (origin, server) = spawn_origin().await;
    let runtime = Arc::new(
        GitHubCopilotRuntime::with_test_origin(
            Secret::new("copilot-replay-token-canary".to_owned()),
            origin,
            NetworkPolicy::Allow,
        )
        .unwrap_or_else(|error| panic!("Copilot test runtime must build: {error}")),
    );
    let provider: Arc<dyn Provider> = Arc::new(
        GitHubCopilotProvider::new(GitHubCopilotProviderConfig {
            name: "copilot-replay-denied".to_owned(),
            model_id: "fixture-model".to_owned(),
            runtime,
        })
        .unwrap_or_else(|error| panic!("Copilot provider must build: {error}")),
    );
    let recorder = Recorder::new(
        provider,
        &directory,
        rw_providers::FixtureRedactor::default(),
    );
    let live = recorder
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("Copilot recording must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(live.iter().all(Result::is_ok));
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("Copilot recording must flush: {error}"));
    let requests = server
        .await
        .unwrap_or_else(|error| panic!("Copilot fixture server must join: {error}"));
    assert_eq!(requests.len(), 2);

    let _network_denied = deny_outbound_network_for_process();
    let replay = ReplayProvider::load("copilot-replay-denied", &directory)
        .await
        .unwrap_or_else(|error| panic!("Copilot replay must load: {error}"));
    let replayed = replay
        .stream(request())
        .await
        .unwrap_or_else(|error| panic!("network-denied Copilot replay must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        serde_json::to_vec(&live)
            .unwrap_or_else(|error| panic!("live events must encode: {error}")),
        serde_json::to_vec(&replayed)
            .unwrap_or_else(|error| panic!("replayed events must encode: {error}"))
    );
    std::fs::remove_dir_all(directory)
        .unwrap_or_else(|error| panic!("fixture directory must remove: {error}"));
}
