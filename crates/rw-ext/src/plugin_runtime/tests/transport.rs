use super::*;

#[tokio::test]
async fn provider_http_redaction_retains_overlap_after_an_earlier_match() {
    let partial = 8;
    let first = format!("prefix {HTTP_SECRET} {}", &HTTP_SECRET[..partial]);
    let second = format!("{} suffix", &HTTP_SECRET[partial..]);
    let mut body: PluginHttpByteStream = Box::pin(futures_util::stream::iter([
        Ok(first.into_bytes()),
        Ok(second.into_bytes()),
    ]));
    let (writer, mut receiver) = RpcWriter::channel();

    let producer = tokio::spawn(async move {
        stream_provider_http_body(
            &RpcId::String("stream-redaction".to_owned()),
            &mut body,
            &CancellationToken::default(),
            &writer,
            &HttpSecretRedactor,
        )
        .await
        .expect("stream redaction");
    });

    let mut rendered = Vec::new();
    while let Some(frame) = receiver.recv_frame().await {
        let RpcFrame::Notification(notification) = frame else {
            panic!("provider HTTP body must emit notifications");
        };
        let params = notification.params.expect("notification params");
        if params.pointer("/event/type").and_then(Value::as_str) == Some("body") {
            let encoded = params
                .pointer("/event/data_base64")
                .and_then(Value::as_str)
                .expect("encoded body");
            rendered.extend(BASE64_STANDARD.decode(encoded).expect("valid body base64"));
        }
    }
    producer.await.expect("HTTP producer");
    assert_eq!(
        String::from_utf8(rendered).expect("UTF-8 fixture"),
        "prefix [REDACTED] [REDACTED] suffix"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn typed_tool_progress_crosses_the_real_reader_without_extending_control_deadlines() {
    #[derive(Default)]
    struct Progress(StdMutex<Vec<String>>);
    impl rw_tools::ToolProgressSink for Progress {
        fn report(&self, update: rw_plugin_protocol::ToolProgress) -> Result<(), ToolError> {
            self.0
                .lock()
                .expect("progress")
                .push(update.message().to_owned());
            Ok(())
        }
    }
    let process = Arc::new(FakeProcess::default());
    let (host_stdin, plugin_input) = tokio::io::duplex(64 * 1024);
    let (mut plugin_output, host_stdout) = tokio::io::duplex(64 * 1024);
    let root = TempDir::new().expect("tempdir");
    let client = JsonRpcPluginClient::start(
        LaunchedPluginProcess {
            stdin: Box::pin(host_stdin),
            stdout: Box::pin(BufReader::new(host_stdout)),
            stderr: Box::pin(BufReader::new(tokio::io::empty())),
            process: process.clone(),
            executable_identity: shell_config(&root).executable_identity().clone(),
        },
        Arc::new(CapabilityEnforcer::new(&manifest(), process.clone())),
        Arc::new(DenyPushHandler),
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(NoopPluginBoundaryRedactor),
        Duration::from_millis(30),
    );
    let progress = Arc::new(Progress::default());
    let result = {
        let client = client.clone();
        let progress = progress.clone();
        tokio::spawn(async move {
            client
                .call_tool(
                    ToolCallParams {
                        name: "fixture".to_owned(),
                        input: json!({}),
                        lifetime: rw_plugin_protocol::OperationLifetime::new(500, 150)
                            .expect("lifetime"),
                    },
                    &CancellationToken::default(),
                    progress,
                    None,
                )
                .await
        })
    };
    let mut input = BufReader::new(plugin_input);
    let mut line = String::new();
    input.read_line(&mut line).await.expect("tool request");
    let request: RpcRequest = serde_json::from_str(line.trim()).expect("tool frame");
    assert_eq!(request.method, METHOD_TOOL_CALL);
    let params: ToolCallParams =
        serde_json::from_value(request.params.expect("params")).expect("typed lifetime");
    assert_eq!(params.lifetime.idle_ms(), 150);
    for sequence in 1..=3 {
        tokio::time::sleep(Duration::from_millis(75)).await;
        let progress_frame = json!({"jsonrpc":"2.0","method":METHOD_TOOL_PROGRESS,"params":{"request_id":request.id,"sequence":sequence,"progress":{"message":format!("step {sequence}")}}});
        plugin_output
            .write_all(format!("{progress_frame}\n").as_bytes())
            .await
            .expect("progress frame");
        tokio::task::yield_now().await;
    }
    let response = json!({"jsonrpc":"2.0","id":request.id,"result":null});
    plugin_output
        .write_all(format!("{response}\n").as_bytes())
        .await
        .expect("outcome");
    assert_eq!(
        result
            .await
            .expect("request task")
            .expect("long tool result"),
        Value::Null
    );
    assert!(!progress.0.lock().expect("progress").is_empty());
    let control = client
        .request("catalog-probe", Value::Null)
        .await
        .expect_err("ordinary control still bounded");
    assert_eq!(control.code, "timeout");
}

#[tokio::test]
async fn credit_refunds_original_wire_bytes_after_rust_json_normalization() {
    let process = Arc::new(FakeProcess::default());
    let (host_stdin, plugin_input) = tokio::io::duplex(64 * 1024);
    let (mut plugin_output, host_stdout) = tokio::io::duplex(64 * 1024);
    let root = TempDir::new().expect("tempdir");
    let client = JsonRpcPluginClient::start(
        LaunchedPluginProcess {
            stdin: Box::pin(host_stdin),
            stdout: Box::pin(BufReader::new(host_stdout)),
            stderr: Box::pin(BufReader::new(tokio::io::empty())),
            process: process.clone(),
            executable_identity: shell_config(&root).executable_identity().clone(),
        },
        Arc::new(CapabilityEnforcer::new(&manifest(), process.clone())),
        Arc::new(DenyPushHandler),
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(NoopPluginBoundaryRedactor),
        Duration::from_secs(2),
    );
    let mut stream = client.provider_stream(json!({})).await.expect("stream");
    let mut input = BufReader::new(plugin_input);
    let mut line = String::new();
    input.read_line(&mut line).await.expect("request");
    let request: RpcRequest = serde_json::from_str(line.trim()).expect("request frame");
    line.clear();
    input.read_line(&mut line).await.expect("initial credit");
    for number in ["0.000001", "100000000000000000000", "1e-7", "1e+21", "-0"] {
        let id = serde_json::to_string(&request.id).expect("id");
        let wire = format!(
            r#"{{"jsonrpc":"2.0","method":"provider/event","params":{{"request_id":{id},"event":{{"type":"tool_call_end","id":"call","arguments":{{"number":{number},"escaped":"\u0061\n\/é"}}}}}}}}"#
        );
        plugin_output
            .write_all(format!("{wire}\n").as_bytes())
            .await
            .expect("write event");
        stream.next().await.expect("event").expect("valid event");
        line.clear();
        tokio::time::timeout(Duration::from_secs(1), input.read_line(&mut line))
            .await
            .expect("credit deadline")
            .expect("credit");
        let frame: RpcNotification = serde_json::from_str(line.trim()).expect("refund");
        let refund: rw_plugin_protocol::ProviderCreditParams =
            serde_json::from_value(frame.params.expect("params")).expect("typed credit");
        assert_eq!(refund.bytes as usize, wire.len());
        assert_eq!(refund.events, 1);
    }
    drop(stream);
    client.settle_effects().await.expect("effects settled");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn full_provider_data_queue_preserves_terminal_and_unrelated_responses() {
    let process = Arc::new(FakeProcess::default());
    let (host_stdin, plugin_input) = tokio::io::duplex(64 * 1024);
    let (mut plugin_output, host_stdout) = tokio::io::duplex(64 * 1024);
    let root = TempDir::new().expect("tempdir");
    let client = JsonRpcPluginClient::start(
        LaunchedPluginProcess {
            stdin: Box::pin(host_stdin),
            stdout: Box::pin(BufReader::new(host_stdout)),
            stderr: Box::pin(BufReader::new(tokio::io::empty())),
            process: process.clone(),
            executable_identity: shell_config(&root).executable_identity().clone(),
        },
        Arc::new(CapabilityEnforcer::new(&manifest(), process.clone())),
        Arc::new(DenyPushHandler),
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(NoopPluginBoundaryRedactor),
        Duration::from_secs(2),
    );
    let mut stream = client.provider_stream(json!({})).await.expect("stream");
    let mut input = BufReader::new(plugin_input);
    let mut line = String::new();
    input.read_line(&mut line).await.expect("request");
    let request: RpcRequest = serde_json::from_str(line.trim()).expect("request frame");
    line.clear();
    input.read_line(&mut line).await.expect("initial credit");
    let credit: RpcNotification = serde_json::from_str(line.trim()).expect("credit frame");
    assert_eq!(credit.method, METHOD_PROVIDER_CREDIT);
    for index in 0..PROVIDER_WINDOW_EVENTS {
        let frame = RpcFrame::Notification(RpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: METHOD_PROVIDER_EVENT.to_owned(),
            params: Some(
                json!({"request_id":request.id,"event":{"type":"text_delta","text":index.to_string()}}),
            ),
        });
        plugin_output
            .write_all(&encode_frame(&frame, MAX_FRAME_BYTES).expect("event"))
            .await
            .expect("write");
    }
    let finished = json!({"type":"finished","reason":"stop"});
    plugin_output
        .write_all(
            &encode_frame(
                &RpcFrame::Notification(RpcNotification {
                    jsonrpc: "2.0".to_owned(),
                    method: METHOD_PROVIDER_EVENT.to_owned(),
                    params: Some(json!({"request_id":request.id,"event":finished})),
                }),
                MAX_FRAME_BYTES,
            )
            .expect("finished"),
        )
        .await
        .expect("write");
    plugin_output
        .write_all(
            &encode_frame(
                &RpcFrame::Success(RpcSuccess {
                    jsonrpc: "2.0".to_owned(),
                    id: request.id,
                    result: Value::Null,
                }),
                MAX_FRAME_BYTES,
            )
            .expect("terminal"),
        )
        .await
        .expect("write");
    let ping = tokio::spawn({
        let client = client.clone();
        async move { client.request("ping", Value::Null).await }
    });
    line.clear();
    input.read_line(&mut line).await.expect("ping");
    let ping_request: RpcRequest = serde_json::from_str(line.trim()).expect("ping frame");
    plugin_output
        .write_all(
            &encode_frame(
                &RpcFrame::Success(RpcSuccess {
                    jsonrpc: "2.0".to_owned(),
                    id: ping_request.id,
                    result: json!("pong"),
                }),
                MAX_FRAME_BYTES,
            )
            .expect("response"),
        )
        .await
        .expect("write");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), ping)
            .await
            .expect("reader stayed live")
            .expect("ping task")
            .expect("ping result"),
        json!("pong")
    );
    for index in 0..PROVIDER_WINDOW_EVENTS {
        assert_eq!(
            stream.next().await.expect("event").expect("valid event")["text"],
            index.to_string()
        );
    }
    assert_eq!(
        stream
            .next()
            .await
            .expect("finished")
            .expect("valid finished"),
        finished
    );
    assert!(stream.next().await.is_none());
    drop(stream);
    assert_eq!(process.killed.load(Ordering::Acquire), 0);
}

#[test]
fn response_ids_are_explicit_and_success_ids_are_non_null() {
    for line in [
        r#"{"jsonrpc":"2.0","result":null}"#,
        r#"{"jsonrpc":"2.0","id":null,"result":null}"#,
        r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"parse error"}}"#,
    ] {
        assert!(
            FrameDecoder::default()
                .push(format!("{line}\n").as_bytes())
                .is_err()
        );
    }
    let frames = FrameDecoder::default()
        .push(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"parse error\"}}\n")
        .expect("explicit parse-error ID");
    assert!(matches!(&frames[0].frame, RpcFrame::Failure(failure) if failure.id.is_none()));
}

#[tokio::test]
async fn undeclared_push_kills_and_prevents_handshake() {
    let root = TempDir::new().expect("tempdir");
    let config = shell_config(&root)
        .with_allowed_domains(["example.com"])
        .expect("network allowlist");
    let manifest = manifest();
    let store = MemoryApproval::default();
    approve_plugin_launch(&store, &manifest, &config, "project:test").expect("approve");
    let process = Arc::new(FakeProcess::default());
    let launcher = MemoryLauncher {
        manifest: manifest.clone(),
        process: process.clone(),
        push: Some(rw_plugin_protocol::METHOD_SESSION_SET_STATUS.to_owned()),
        hang_method: None,
    };
    let result = PluginHost::launch_approved(
        &launcher,
        &store,
        &config,
        "project:test",
        &[root.path().to_path_buf()],
        manifest,
        Arc::new(DenyPushHandler),
        Arc::new(NoopPluginBoundaryRedactor),
    )
    .await;
    assert!(result.is_err());
    assert!(process.killed.load(Ordering::Acquire) >= 1);
}

#[tokio::test]
async fn redaction_is_mandatory_for_hook_event_and_incoming_push_values() {
    let process = Arc::new(FakeProcess::default());
    let (host_stdin, plugin_input) = tokio::io::duplex(16 * 1024);
    let (plugin_output, host_stdout) = tokio::io::duplex(16 * 1024);
    let pushes = Arc::new(RecordingPush::default());
    tokio::spawn(async move {
        let mut input = BufReader::new(plugin_input);
        let mut output = plugin_output;
        let mut line = String::new();
        input.read_line(&mut line).await.expect("hook request");
        assert!(!line.contains("PLUGIN_CANARY_SECRET"));
        let request: RpcRequest = serde_json::from_str(line.trim()).expect("hook frame");
        output
            .write_all(
                &encode_frame(
                    &RpcFrame::Success(RpcSuccess {
                        jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                        id: request.id,
                        result: Value::Null,
                    }),
                    MAX_FRAME_BYTES,
                )
                .expect("hook response"),
            )
            .await
            .expect("write hook response");
        line.clear();
        input
            .read_line(&mut line)
            .await
            .expect("event notification");
        assert!(!line.contains("PLUGIN_CANARY_SECRET"));
        output
            .write_all(
                &encode_frame(
                    &RpcFrame::Request(RpcRequest {
                        jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                        id: RpcId::String("push-canary".to_owned()),
                        method: METHOD_UI_NOTIFY.to_owned(),
                        params: Some(json!({
                            "title":"canary",
                            "message":"PLUGIN_CANARY_SECRET"
                        })),
                    }),
                    MAX_FRAME_BYTES,
                )
                .expect("push frame"),
            )
            .await
            .expect("write push");
    });
    let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process.clone()));
    let identity = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
        .expect("shell")
        .executable_identity()
        .clone();
    let client = JsonRpcPluginClient::start(
        LaunchedPluginProcess {
            stdin: Box::pin(host_stdin),
            stdout: Box::pin(BufReader::new(host_stdout)),
            stderr: Box::pin(BufReader::new(tokio::io::empty())),
            process,
            executable_identity: identity,
        },
        enforcer,
        pushes.clone(),
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(CanaryRedactor),
        Duration::from_secs(1),
    );
    client
        .request(
            rw_plugin_protocol::METHOD_HOOK_INVOKE,
            json!({"payload":"PLUGIN_CANARY_SECRET"}),
        )
        .await
        .expect("redacted hook request");
    client
        .notify(
            METHOD_EVENT_PUBLISH,
            json!({"payload":"PLUGIN_CANARY_SECRET"}),
        )
        .await
        .expect("redacted event notification");
    tokio::time::timeout(Duration::from_secs(1), async {
        while pushes.0.lock().expect("push lock").is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("push deadline");
    assert_eq!(
        pushes.0.lock().expect("push lock")[0].1["message"],
        "[REDACTED]"
    );
}

#[tokio::test]
async fn plugin_originated_undeclared_push_is_killed_and_reaped() {
    let (sdk, config) = sdk_fixture_config("undeclared-push.ts");
    let manifest = PluginManifest {
        name: "undeclared-push".to_owned(),
        version: "1.0.0".to_owned(),
        protocol: rw_plugin_protocol::PROTOCOL_VERSION,
        capabilities: PluginCapabilities::default(),
    };
    let store = MemoryApproval::default();
    approve_plugin_launch(&store, &manifest, &config, "conformance:violation")
        .expect("approve adversarial fixture");
    let launcher = TrackingDirectLauncher::default();
    let host_result = PluginHost::launch_approved(
        &launcher,
        &store,
        &config,
        "conformance:violation",
        &[sdk],
        manifest,
        Arc::new(DenyPushHandler),
        Arc::new(NoopPluginBoundaryRedactor),
    )
    .await;
    if let Ok(host) = &host_result {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !host.enforcer().violated() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("capability violation deadline");
    }
    let process = launcher
        .0
        .lock()
        .expect("tracking launcher")
        .clone()
        .expect("tracked process");
    tokio::time::timeout(Duration::from_secs(2), process.wait())
        .await
        .expect("violator reap deadline")
        .expect("violator wait");
}
