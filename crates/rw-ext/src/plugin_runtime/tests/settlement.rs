use super::*;

#[tokio::test]
async fn ordinary_request_cancellation_settles_parent_and_child_effects() {
    let root = TempDir::new().expect("tempdir");
    let client = mutating_child_client(&root, Duration::from_secs(5)).await;
    let cancellation = CancellationToken::default();
    let task = {
        let client = Arc::clone(&client);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            client
                .request_cancellable(
                    rw_plugin_protocol::METHOD_HOOK_INVOKE,
                    Value::Null,
                    &cancellation,
                )
                .await
        })
    };
    wait_for_mutation(&root).await;
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .expect("settlement deadline")
        .expect("request task")
        .expect_err("cancelled request");
    assert_eq!(error.code, "cancelled");
    assert_eq!(
        client
            .request("next", Value::Null)
            .await
            .expect_err("closed client")
            .code,
        "closed"
    );
    assert_conflicting_writes_are_safe(&root).await;
}

#[tokio::test]
async fn dropped_hook_request_settles_parent_and_child_effects() {
    let root = TempDir::new().expect("tempdir");
    let client = mutating_child_client(&root, Duration::from_secs(5)).await;
    let task = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .request(rw_plugin_protocol::METHOD_HOOK_INVOKE, Value::Null)
                .await
        })
    };
    wait_for_mutation(&root).await;
    task.abort();
    assert!(task.await.expect_err("dropped caller").is_cancelled());
    tokio::time::timeout(Duration::from_secs(4), client.settle_effects())
        .await
        .expect("drop settlement")
        .expect("effects settled");
    assert_conflicting_writes_are_safe(&root).await;
}

#[tokio::test]
async fn typed_tool_idle_timeout_settles_parent_and_child_effects() {
    let root = TempDir::new().expect("tempdir");
    let client = mutating_child_client(&root, Duration::from_secs(5)).await;
    let task = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .call_tool(
                    ToolCallParams {
                        name: "fixture".to_owned(),
                        input: json!({}),
                        lifetime: rw_plugin_protocol::OperationLifetime::new(5000, 500)
                            .expect("lifetime"),
                    },
                    &CancellationToken::default(),
                    Arc::new(rw_tools::NoopProgressSink),
                    None,
                )
                .await
        })
    };
    wait_for_mutation(&root).await;
    let error = tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .expect("settlement deadline")
        .expect("request task")
        .expect_err("idle timeout");
    assert_eq!(error.code, "timeout");
    assert!(error.message.contains("idle"));
    assert_conflicting_writes_are_safe(&root).await;
}

#[tokio::test]
async fn ordinary_request_timeout_settles_parent_and_child_effects() {
    let root = TempDir::new().expect("tempdir");
    let client = mutating_child_client(&root, Duration::from_millis(200)).await;
    let task = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .request(rw_plugin_protocol::METHOD_HOOK_INVOKE, Value::Null)
                .await
        })
    };
    wait_for_mutation(&root).await;
    let error = tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .expect("settlement deadline")
        .expect("request task")
        .expect_err("timed out request");
    assert_eq!(error.code, "timeout");
    assert_conflicting_writes_are_safe(&root).await;
}

#[tokio::test]
async fn dropped_provider_stream_settles_real_parent_and_child_effects() {
    let root = TempDir::new().expect("tempdir");
    let client = mutating_child_client(&root, Duration::from_secs(5)).await;
    let stream = client
        .provider_stream(json!({"alias":"fixture/model", "request":{}}))
        .await
        .expect("provider admission");
    wait_for_mutation(&root).await;
    drop(stream);
    tokio::time::timeout(Duration::from_secs(4), client.settle_effects())
        .await
        .expect("provider local effects settled")
        .expect("effects settled");
    assert_conflicting_writes_are_safe(&root).await;
}

#[tokio::test]
async fn approved_handshake_registers_custom_tool_and_reaps_on_shutdown() {
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
        push: None,
        hang_method: None,
    };
    let host = Arc::new(
        PluginHost::launch_approved(
            &launcher,
            &store,
            &config,
            "project:test",
            &[root.path().to_path_buf()],
            manifest.clone(),
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await
        .expect("launch"),
    );
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(
            RpcToolAdapter::new(
                manifest.capabilities.tools[0].clone(),
                ready_endpoint(&host),
            )
            .expect("approved adapter"),
        ))
        .expect("register custom tool");
    let tool = registry.resolve("fixture_tool").expect("resolved tool");
    assert_eq!(
        tool.descriptor().capabilities,
        CapabilityManifest::new([ToolCapability::ReadFilesystem])
    );
    let context = ToolContext::new(root.path()).expect("tool context");
    let result = tool
        .execute(&context, json!({}))
        .await
        .expect("tool result");
    assert_eq!(result.content, "fixture");
    host.shutdown().await.expect("shutdown");
    assert!(process.waited.load(Ordering::Acquire) >= 1);
}

#[tokio::test]
async fn dropping_launched_host_kills_process_without_explicit_shutdown() {
    let root = TempDir::new().expect("tempdir");
    let config = shell_config(&root)
        .with_allowed_domains(["example.com"])
        .expect("network allowlist");
    let manifest = manifest();
    let store = MemoryApproval::default();
    approve_plugin_launch(&store, &manifest, &config, "project:drop").expect("approve");
    let process = Arc::new(FakeProcess::default());
    let host = PluginHost::launch_approved(
        &MemoryLauncher {
            manifest: manifest.clone(),
            process: process.clone(),
            push: None,
            hang_method: None,
        },
        &store,
        &config,
        "project:drop",
        &[root.path().to_path_buf()],
        manifest,
        Arc::new(DenyPushHandler),
        Arc::new(NoopPluginBoundaryRedactor),
    )
    .await
    .expect("launch");

    drop(host);

    assert!(
        process.killed.load(Ordering::Acquire) >= 1,
        "the final client owner must terminate an unshut plugin"
    );
}

#[tokio::test]
async fn shutdown_uses_effect_proof_instead_of_kill_attempt_outcome() {
    for blocked in [false, true] {
        let root = TempDir::new().expect("tempdir");
        let config = shell_config(&root)
            .with_allowed_domains(["example.com"])
            .expect("allowlist");
        let manifest = manifest();
        let approvals = MemoryApproval::default();
        approve_plugin_launch(&approvals, &manifest, &config, "project:shutdown").expect("approve");
        let process = Arc::new(FakeProcess::default());
        let launcher = MemoryLauncher {
            manifest: manifest.clone(),
            process: Arc::clone(&process),
            push: None,
            hang_method: None,
        };
        let host = PluginHost::launch_approved(
            &launcher,
            &approvals,
            &config,
            "project:shutdown",
            &[root.path().to_path_buf()],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await
        .expect("launch");
        process.kill_fails.store(true, Ordering::Release);
        process.settlement_blocked.store(blocked, Ordering::Release);
        let result = host.client.shutdown(Duration::from_millis(30)).await;
        assert_eq!(result.is_err(), blocked);
        assert_eq!(
            host.client.shutdown_complete.load(Ordering::Acquire),
            !blocked
        );
        assert!(process.killed.load(Ordering::Acquire) > 0);
        if blocked {
            process.settlement_release.notify_one();
            host.shutdown()
                .await
                .expect("owned cleanup continues after API timeout");
        }
        assert!(process.waited.load(Ordering::Acquire) > 0);
    }
}

#[tokio::test]
async fn request_timeout_is_bounded_and_shutdown_still_kills() {
    let process = Arc::new(FakeProcess::default());
    let (host_stdin, mut plugin_input) = tokio::io::duplex(4096);
    let (_plugin_output, host_stdout) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        let mut bytes = [0u8; 1024];
        let _ = tokio::io::AsyncReadExt::read(&mut plugin_input, &mut bytes).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process.clone()));
    let client = JsonRpcPluginClient::start(
        LaunchedPluginProcess {
            stdin: Box::pin(host_stdin),
            stdout: Box::pin(BufReader::new(host_stdout)),
            stderr: Box::pin(BufReader::new(tokio::io::empty())),
            process: process.clone(),
            executable_identity: PluginProcessConfig::new(PathBuf::from("/bin/sh"))
                .expect("shell")
                .executable_identity()
                .clone(),
        },
        enforcer,
        Arc::new(DenyPushHandler),
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(NoopPluginBoundaryRedactor),
        Duration::from_millis(30),
    );
    let error = client
        .request("hang", Value::Null)
        .await
        .expect_err("timeout");
    assert_eq!(error.code, "timeout");
    client
        .shutdown(Duration::from_millis(30))
        .await
        .expect("bounded kill/reap");
    assert!(process.killed.load(Ordering::Acquire) >= 1);
}

#[tokio::test]
async fn panicked_host_command_cannot_release_its_settlement_barrier() {
    let process = Arc::new(FakeProcess::default());
    let (host_stdin, _plugin_input) = tokio::io::duplex(4096);
    let (mut plugin_output, host_stdout) = tokio::io::duplex(4096);
    let push = Arc::new(DelayedActorPush {
        panic_after_admission: true,
        ..Default::default()
    });
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
        push.clone(),
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(NoopPluginBoundaryRedactor),
        Duration::from_secs(5),
    );
    let frame = RpcFrame::Request(RpcRequest {
        jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
        id: RpcId::String("panic-after-admission".to_owned()),
        method: METHOD_UI_NOTIFY.to_owned(),
        params: Some(json!({"title":"fixture", "message":"fixture"})),
    });
    plugin_output
        .write_all(&encode_frame(&frame, MAX_FRAME_BYTES).expect("encode"))
        .await
        .expect("write");
    push.started.notified().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while process.killed.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("panic started teardown");
    let mut settlement = tokio::spawn(async move { client.settle_effects().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut settlement)
            .await
            .is_err()
    );
    push.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !push.committed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("already admitted actor work can still commit");
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut settlement)
            .await
            .is_err()
    );
    settlement.abort();
    let _ = settlement.await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn ordinary_cancellation_drains_admitted_host_push_before_reporting_settlement() {
    let process = Arc::new(FakeProcess::default());
    let (host_stdin, plugin_input) = tokio::io::duplex(4096);
    let (mut plugin_output, host_stdout) = tokio::io::duplex(4096);
    let push = Arc::new(DelayedActorPush::default());
    let root = TempDir::new().expect("tempdir");
    let client = JsonRpcPluginClient::start(
        LaunchedPluginProcess {
            stdin: Box::pin(host_stdin),
            stdout: Box::pin(BufReader::new(host_stdout)),
            stderr: Box::pin(BufReader::new(tokio::io::empty())),
            process: process.clone(),
            executable_identity: shell_config(&root).executable_identity().clone(),
        },
        Arc::new(CapabilityEnforcer::new(&manifest(), process)),
        push.clone(),
        Arc::new(DenyPluginProviderHttpHandler),
        Arc::new(NoopPluginBoundaryRedactor),
        Duration::from_secs(5),
    );
    let cancellation = CancellationToken::default();
    let mut task = {
        let client = Arc::clone(&client);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            client
                .request_cancellable(
                    rw_plugin_protocol::METHOD_HOOK_INVOKE,
                    Value::Null,
                    &cancellation,
                )
                .await
        })
    };
    let mut input = BufReader::new(plugin_input);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), input.read_line(&mut line))
        .await
        .expect("request deadline")
        .expect("request frame");
    let frame = RpcFrame::Request(RpcRequest {
        jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
        id: RpcId::String("admitted-actor-command".to_owned()),
        method: rw_plugin_protocol::METHOD_UI_NOTIFY.to_owned(),
        params: Some(json!({"title":"fixture", "message":"fixture"})),
    });
    plugin_output
        .write_all(&encode_frame(&frame, MAX_FRAME_BYTES).expect("push frame"))
        .await
        .expect("plugin push");
    tokio::time::timeout(Duration::from_secs(2), push.started.notified())
        .await
        .expect("push admitted");
    // A delayed actor command must not block unrelated response correlation.
    let ping = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.request("ping", Value::Null).await })
    };
    line.clear();
    tokio::time::timeout(Duration::from_secs(1), input.read_line(&mut line))
        .await
        .expect("ping write deadline")
        .expect("ping request");
    let ping_request: RpcFrame = serde_json::from_str(line.trim()).expect("ping frame");
    let RpcFrame::Request(ping_request) = ping_request else {
        panic!("expected request")
    };
    plugin_output
        .write_all(
            &encode_frame(
                &RpcFrame::Success(RpcSuccess {
                    jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                    id: ping_request.id,
                    result: json!("pong"),
                }),
                MAX_FRAME_BYTES,
            )
            .expect("ping response"),
        )
        .await
        .expect("write ping response");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), ping)
            .await
            .expect("reader remained live")
            .expect("ping task")
            .expect("ping result"),
        json!("pong")
    );
    cancellation.cancel();
    assert!(
        tokio::time::timeout(
            DEFAULT_REQUEST_TIMEOUT + Duration::from_millis(100),
            &mut task
        )
        .await
        .is_err()
    );
    assert!(!push.committed.load(Ordering::Acquire));
    push.release.notify_one();
    let failure = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("settlement deadline")
        .expect("request task")
        .expect_err("cancelled");
    assert_eq!(failure.code, "cancelled");
    assert!(push.committed.load(Ordering::Acquire));
}

#[tokio::test]
async fn ordinary_cancellation_drops_host_http_even_when_handler_ignores_token() {
    let process = Arc::new(FakeProcess::default());
    let (host_stdin, plugin_input) = tokio::io::duplex(4096);
    let (mut plugin_output, host_stdout) = tokio::io::duplex(4096);
    let http = Arc::new(IgnoringCancellationHttp::default());
    let mut approved = manifest();
    approved.capabilities.providers[0].credential_references = vec!["fixture-token".to_owned()];
    let root = TempDir::new().expect("tempdir");
    let client = JsonRpcPluginClient::start(
        LaunchedPluginProcess {
            stdin: Box::pin(host_stdin),
            stdout: Box::pin(BufReader::new(host_stdout)),
            stderr: Box::pin(BufReader::new(tokio::io::empty())),
            process: process.clone(),
            executable_identity: shell_config(&root).executable_identity().clone(),
        },
        Arc::new(CapabilityEnforcer::new(&approved, process)),
        Arc::new(DenyPushHandler),
        http.clone(),
        Arc::new(NoopPluginBoundaryRedactor),
        Duration::from_secs(5),
    );
    let cancellation = CancellationToken::default();
    let task = {
        let client = Arc::clone(&client);
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            client
                .request_cancellable(
                    rw_plugin_protocol::METHOD_HOOK_INVOKE,
                    Value::Null,
                    &cancellation,
                )
                .await
        })
    };
    let mut input = BufReader::new(plugin_input);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), input.read_line(&mut line))
        .await
        .expect("request deadline")
        .expect("request frame");
    let frame = RpcFrame::Request(RpcRequest {
        jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
        id: RpcId::String("http-owned-effect".to_owned()),
        method: METHOD_PROVIDER_HTTP.to_owned(),
        params: Some(json!({
            "alias": "fixture/model", "credential_reference": "fixture-token",
            "request": {"url": "https://example.test", "method": "GET", "credential_header": "Authorization"}
        })),
    });
    plugin_output
        .write_all(&encode_frame(&frame, MAX_FRAME_BYTES).expect("HTTP frame"))
        .await
        .expect("plugin HTTP request");
    tokio::time::timeout(Duration::from_secs(2), http.started.notified())
        .await
        .expect("HTTP started");
    cancellation.cancel();
    let failure = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("settlement deadline")
        .expect("request task")
        .expect_err("cancelled");
    assert_eq!(failure.code, "cancelled");
    assert!(http.dropped.load(Ordering::Acquire));
    assert!(
        client
            .termination
            .active_provider_http
            .lock()
            .expect("HTTP state")
            .is_empty()
    );
}

#[tokio::test]
async fn reader_exit_cancels_and_drains_active_provider_http() {
    let process = Arc::new(FakeProcess::default());
    let (plugin_output, host_stdout) = tokio::io::duplex(1024);
    drop(plugin_output);
    let (writer, _receiver) = RpcWriter::channel();
    let active_provider_http = Arc::new(StdMutex::new(BTreeMap::new()));
    let cancellation = CancellationToken::default();
    active_provider_http
        .lock()
        .expect("active HTTP lock")
        .insert(
            RpcId::String("active-http".to_owned()),
            cancellation.clone(),
        );
    let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process.clone()));
    let termination = Arc::new(RequestTermination {
        process: process.clone(),
        closed: Arc::new(AtomicBool::new(false)),
        in_flight: Arc::new(Semaphore::new(WRITER_QUEUE_CAPACITY)),
        active_provider_http: Arc::clone(&active_provider_http),
        cancellation: CancellationToken::default(),
        host_effects: Arc::new(Semaphore::new(HOST_EFFECT_CAPACITY as usize)),
        completion: StdMutex::new(None),
    });
    let state = ReaderState {
        termination,
        writer,
        pending: Arc::new(Mutex::new(BTreeMap::new())),
        provider_streams: Arc::new(StdMutex::new(BTreeMap::new())),
        provider_http: Arc::new(DenyPluginProviderHttpHandler),
        active_provider_http: Arc::clone(&active_provider_http),
        enforcer,
        push_handler: Arc::new(DenyPushHandler),
        host_commands: Arc::new(StdMutex::new(BTreeSet::new())),
        redactor: Arc::new(NoopPluginBoundaryRedactor),
        process: process.clone(),
    };

    reader_loop(Box::pin(BufReader::new(host_stdout)), state).await;

    assert!(cancellation.is_cancelled());
    assert!(
        active_provider_http
            .lock()
            .expect("active HTTP lock")
            .is_empty()
    );
    assert!(process.killed.load(Ordering::Acquire) >= 1);
}
