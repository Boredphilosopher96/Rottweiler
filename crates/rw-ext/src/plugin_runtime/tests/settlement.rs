use super::*;

#[derive(Clone, Copy)]
enum InitializationBoundary {
    Deadline,
    Cancellation,
}

struct InitializationBoundaryLauncher {
    manifest: PluginManifest,
    process: Arc<FakeProcess>,
    boundary: InitializationBoundary,
}

#[async_trait]
impl PluginLauncher for InitializationBoundaryLauncher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
        activation: &crate::PluginActivation,
    ) -> Result<LaunchedPluginProcess, PluginLaunchError> {
        let launched = MemoryLauncher {
            manifest: self.manifest.clone(),
            process: Arc::clone(&self.process),
            push: None,
            hang_method: None,
        }
        .launch(config, profile, activation)
        .await?;
        match self.boundary {
            InitializationBoundary::Deadline => {
                tokio::time::advance(Duration::from_secs(1)).await;
            }
            InitializationBoundary::Cancellation => activation.cancellation().cancel(),
        }
        Ok(launched)
    }
}

#[tokio::test(start_paused = true)]
async fn expired_or_cancelled_activation_cannot_enqueue_initialization() {
    for (boundary, expected_code) in [
        (InitializationBoundary::Deadline, "timeout"),
        (InitializationBoundary::Cancellation, "cancelled"),
    ] {
        let root = TempDir::new().expect("tempdir");
        let config = shell_config(&root)
            .with_allowed_domains(["example.com"])
            .expect("domains");
        let expected = manifest();
        let approvals = MemoryApproval::default();
        approve_plugin_launch(
            &approvals,
            &expected,
            &config,
            "project:initialization-boundary",
        )
        .expect("approve");
        let process = Arc::new(FakeProcess::default());
        let cancellation = CancellationToken::default();
        let activation = crate::PluginActivation::until(
            cancellation,
            tokio::time::Instant::now() + Duration::from_secs(1),
        );
        let result = PluginHost::launch_approved(
            &InitializationBoundaryLauncher {
                manifest: expected.clone(),
                process: Arc::clone(&process),
                boundary,
            },
            Arc::new(approvals),
            &config,
            "project:initialization-boundary",
            &[root.path().to_path_buf()],
            expected,
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
            &activation,
        )
        .await;
        assert!(
            matches!(result, Err(PluginHostError::Rpc(ref error)) if error.code == expected_code)
        );
        assert_eq!(
            process.initialize_requests.load(Ordering::Acquire),
            0,
            "revoked initialization reached the transport"
        );
        assert!(
            process.waited.load(Ordering::Acquire) > 0,
            "accepted child was not physically retired"
        );
    }
}

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
            Arc::new(store),
            &config,
            "project:test",
            &[root.path().to_path_buf()],
            manifest.clone(),
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
            &crate::PluginActivation::new(rw_tools::CancellationToken::default()),
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
        Arc::new(store),
        &config,
        "project:drop",
        &[root.path().to_path_buf()],
        manifest,
        Arc::new(DenyPushHandler),
        Arc::new(NoopPluginBoundaryRedactor),
        &crate::PluginActivation::new(rw_tools::CancellationToken::default()),
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
            Arc::new(approvals),
            &config,
            "project:shutdown",
            &[root.path().to_path_buf()],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
            &crate::PluginActivation::new(rw_tools::CancellationToken::default()),
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
    let error = tokio::time::timeout(Duration::from_secs(1), client.settle_effects())
        .await
        .expect("failed proof wakes waiters")
        .expect_err("panic is not settled");
    assert_eq!(error.code, "effects_unsettled");
    assert_eq!(
        client.termination.host_effects.available_permits(),
        HOST_EFFECT_CAPACITY as usize - 1
    );
    push.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !push.committed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("already admitted actor work can still commit");
    assert_eq!(
        client
            .settle_effects()
            .await
            .expect_err("failed proof is sticky")
            .code,
        "effects_unsettled"
    );
    assert_eq!(
        client.termination.host_effects.available_permits(),
        HOST_EFFECT_CAPACITY as usize - 1
    );
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
async fn ordinary_cancellation_retains_host_http_until_explicit_effect_proof() {
    http_cancellation_proof(false).await;
}

#[tokio::test(start_paused = true)]
async fn expired_http_proof_reports_failure_but_eventual_cleanup_returns_capacity() {
    http_cancellation_proof(true).await;
}

struct HttpCancellationFixture {
    client: Arc<JsonRpcPluginClient>,
    http: Arc<IgnoringCancellationHttp>,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<Result<Value, PluginRpcError>>,
    _root: TempDir,
    _input: BufReader<tokio::io::DuplexStream>,
    _output: tokio::io::DuplexStream,
}

async fn admitted_http_cancellation() -> HttpCancellationFixture {
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
                    rw_plugin_protocol::METHOD_PROVIDER_MODELS,
                    json!({"alias_prefix":"fixture/"}),
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
    let RpcFrame::Request(invocation) = serde_json::from_str(&line).expect("catalog frame") else {
        panic!("catalog request")
    };
    let frame = RpcFrame::Request(RpcRequest {
        jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
        id: RpcId::String("http-owned-effect".to_owned()),
        method: METHOD_PROVIDER_HTTP.to_owned(),
        params: Some(json!({
            "invocation_id": invocation.id, "alias": "fixture/", "credential_reference": "fixture-token",
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
    HttpCancellationFixture {
        client,
        http,
        cancellation,
        task,
        _root: root,
        _input: input,
        _output: plugin_output,
    }
}

async fn http_cancellation_proof(expire: bool) {
    let HttpCancellationFixture {
        client,
        http,
        cancellation,
        task,
        _root,
        _input,
        _output,
    } = admitted_http_cancellation().await;
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(2), http.settling.notified())
        .await
        .expect("proof begins");
    assert!(http.dropped.load(Ordering::Acquire));
    assert!(
        !task.is_finished(),
        "response future drop is not physical HTTP proof"
    );
    assert_eq!(
        client
            .termination
            .active_provider_http
            .lock()
            .expect("HTTP ownership")
            .len(),
        1
    );
    if expire {
        tokio::time::advance(Duration::from_secs(6)).await;
        http.settling.notified().await; // Same owned operation resumes its proof.
    } else {
        http.release.notify_one();
    }
    let failure = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("settlement deadline")
        .expect("request task")
        .expect_err("cancelled");
    assert_eq!(
        failure.code,
        if expire {
            "effects_unsettled"
        } else {
            "cancelled"
        }
    );
    if expire {
        assert_eq!(
            client.termination.host_effects.available_permits(),
            HOST_EFFECT_CAPACITY as usize - 1
        );
        http.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while client.termination.host_effects.available_permits()
                != HOST_EFFECT_CAPACITY as usize
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("eventual proof returns physical capacity");
        assert_eq!(
            client
                .settle_effects()
                .await
                .expect_err("host failure remains sticky")
                .code,
            "effects_unsettled"
        );
    }
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
async fn reader_exit_cancels_without_discarding_active_provider_http_ownership() {
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
            super::super::provider_http::ActiveHttp {
                invocation: RpcId::Number(1),
                cancellation: cancellation.clone(),
                settled: Arc::new(AtomicBool::new(false)),
            },
        );
    let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process.clone()));
    let termination = Arc::new(RequestTermination {
        process: process.clone(),
        closed: Arc::new(AtomicBool::new(false)),
        in_flight: Arc::new(Semaphore::new(WRITER_QUEUE_CAPACITY)),
        active_provider_http: Arc::clone(&active_provider_http),
        cancellation: CancellationToken::default(),
        host_effects: Arc::new(Semaphore::new(HOST_EFFECT_CAPACITY as usize)),
        host_failure: watch::channel(false).0,
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
    assert_eq!(
        active_provider_http.lock().expect("active HTTP lock").len(),
        1,
        "cancellation cannot erase an operation's identity before its owner proves settlement"
    );
    assert!(process.killed.load(Ordering::Acquire) >= 1);
}

struct PausedApprovalRead {
    started: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: StdMutex<std::sync::mpsc::Receiver<()>>,
}
impl ApprovalStore for PausedApprovalRead {
    fn approved_fingerprint(&self, _: &str) -> Result<Option<String>, ApprovalStoreError> {
        if let Some(started) = self.started.lock().expect("start owner").take() {
            let _ = started.send(());
        }
        self.release
            .lock()
            .expect("read owner")
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| ApprovalStoreError {
                message: error.to_string(),
            })?;
        Ok(None)
    }
    fn record_approval(&self, _: &str, _: &str) -> Result<(), ApprovalStoreError> {
        panic!("verification cannot write approval")
    }
}

#[tokio::test]
async fn cancelled_launch_keeps_blocking_approval_owner_without_blocking_callbacks() {
    let root = TempDir::new().expect("root");
    let config = shell_config(&root);
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let store = Arc::new(PausedApprovalRead {
        started: StdMutex::new(Some(started)),
        release: StdMutex::new(released),
    });
    let retained = Arc::downgrade(&store);
    let process = Arc::new(FakeProcess::default());
    let launcher = MemoryLauncher {
        manifest: manifest(),
        process: process.clone(),
        push: None,
        hang_method: None,
    };
    let launch = tokio::spawn(async move {
        PluginHost::launch_approved(
            &launcher,
            store,
            &config,
            "paused-verification",
            &[root.path().to_path_buf()],
            manifest(),
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
            &crate::PluginActivation::new(rw_tools::CancellationToken::default()),
        )
        .await
    });
    entered.await.expect("verification entered physical worker");
    // A synchronous implementation stalls this current-thread executor until
    // the store's safety timeout and the launch has already returned an error.
    tokio::task::yield_now().await;
    assert!(
        !launch.is_finished(),
        "approval IO must leave callbacks serviceable"
    );
    launch.abort();
    assert!(matches!(launch.await, Err(error) if error.is_cancelled()));
    assert!(
        retained.upgrade().is_some(),
        "caller drop cannot release the active read owner"
    );
    release.send(()).expect("finish actual read");
    tokio::time::timeout(Duration::from_secs(2), async {
        while retained.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("physical read retires its owner");
    assert_eq!(
        process.killed.load(Ordering::Acquire),
        0,
        "cancelled verification never launches a process"
    );
}

#[tokio::test]
async fn initialization_cancellation_and_caller_loss_keep_admission_until_retirement() {
    for drop_caller in [false, true] {
        let root = TempDir::new().expect("tempdir");
        let config = shell_config(&root)
            .with_allowed_domains(["example.com"])
            .expect("domains");
        let manifest = manifest();
        let approvals = MemoryApproval::default();
        approve_plugin_launch(&approvals, &manifest, &config, "project:initialization")
            .expect("approve");
        let admission = Arc::new(Semaphore::new(1));
        let process = Arc::new(FakeProcess::default());
        process.settlement_blocked.store(true, Ordering::Release);
        *process.retirement_credit.lock().expect("credit") =
            Some(Arc::clone(&admission).acquire_owned().await.expect("slot"));
        let cancellation = CancellationToken::default();
        let task = tokio::spawn({
            let process = Arc::clone(&process);
            let cancellation = cancellation.clone();
            let workspace = root.path().to_path_buf();
            async move {
                PluginHost::launch_approved(
                    &MemoryLauncher {
                        manifest: manifest.clone(),
                        process,
                        push: None,
                        hang_method: Some(METHOD_INITIALIZE.to_owned()),
                    },
                    Arc::new(approvals),
                    &config,
                    "project:initialization",
                    &[workspace],
                    manifest,
                    Arc::new(DenyPushHandler),
                    Arc::new(NoopPluginBoundaryRedactor),
                    &crate::PluginActivation::new(cancellation.clone()),
                )
                .await
            }
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            process.initialize_received.notified(),
        )
        .await
        .expect("initialize reached actual transport");
        cancellation.cancel();
        // This must precede the independent five-second RPC timer.
        tokio::time::timeout(
            Duration::from_secs(1),
            process.retirement_started.notified(),
        )
        .await
        .expect("cancellation starts physical retirement");
        assert!(process.killed.load(Ordering::Acquire) > 0);
        assert!(
            !task.is_finished(),
            "cancellation cannot claim early settlement"
        );
        assert_eq!(admission.available_permits(), 0);
        assert_eq!(process.waited.load(Ordering::Acquire), 0);
        let task = if drop_caller {
            task.abort();
            let Err(error) = task.await else {
                panic!("caller was not dropped");
            };
            assert!(error.is_cancelled());
            None
        } else {
            Some(task)
        };
        process.settlement_blocked.store(false, Ordering::Release);
        process.settlement_release.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), async {
            while admission.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual retired process refunds admission");
        assert!(process.waited.load(Ordering::Acquire) > 0);
        if let Some(task) = task {
            let result = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("launch settles")
                .expect("launch task");
            assert!(
                matches!(result, Err(PluginHostError::Rpc(ref error)) if error.code == "cancelled")
            );
        }
    }
}

#[tokio::test]
async fn failed_initialization_joins_admitted_host_callback_before_returning() {
    let root = TempDir::new().expect("tempdir");
    let config = shell_config(&root)
        .with_allowed_domains(["example.com"])
        .expect("domains");
    let expected = manifest();
    let approvals = MemoryApproval::default();
    approve_plugin_launch(
        &approvals,
        &expected,
        &config,
        "project:initializing-callback",
    )
    .expect("approve");
    let process = Arc::new(FakeProcess::default());
    let push = Arc::new(DelayedActorPush::default());
    let task = tokio::spawn({
        let process = Arc::clone(&process);
        let push = Arc::clone(&push);
        let workspace = root.path().to_path_buf();
        async move {
            let mut returned = expected.clone();
            returned.name = "different-initialized-plugin".to_owned();
            PluginHost::launch_approved(
                &MemoryLauncher {
                    manifest: returned,
                    process,
                    push: Some((
                        METHOD_UI_NOTIFY.to_owned(),
                        json!({"title":"fixture","message":"hello"}),
                    )),
                    hang_method: None,
                },
                Arc::new(approvals),
                &config,
                "project:initializing-callback",
                &[workspace],
                expected,
                push,
                Arc::new(NoopPluginBoundaryRedactor),
                &crate::PluginActivation::new(CancellationToken::default()),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), push.started.notified())
        .await
        .expect("host callback admitted");
    tokio::time::timeout(
        Duration::from_secs(1),
        process.retirement_started.notified(),
    )
    .await
    .expect("manifest mismatch starts retirement");
    assert!(
        process.waited.load(Ordering::Acquire) > 0,
        "physical child is already reaped"
    );
    assert!(
        !task.is_finished(),
        "child exit is not host callback settlement"
    );
    assert!(!push.committed.load(Ordering::Acquire));
    push.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("owned callback settles")
        .expect("launch task");
    assert!(
        matches!(result, Err(PluginHostError::Approval(ref message)) if message.contains("differs"))
    );
    assert!(push.committed.load(Ordering::Acquire));
}
