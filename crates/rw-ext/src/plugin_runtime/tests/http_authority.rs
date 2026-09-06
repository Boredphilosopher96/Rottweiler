use super::*;

#[tokio::test]
async fn provider_http_rejects_unrelated_invocations_before_touching_credentials() {
    for (method, alias, wrong_id) in [
        (METHOD_PROVIDER_MODELS, "fixture/model", false),
        (METHOD_PROVIDER_MODELS, "fixture/", true),
        (rw_plugin_protocol::METHOD_HOOK_INVOKE, "fixture/", false),
    ] {
        let process = Arc::new(FakeProcess::default());
        let (host_stdin, plugin_input) = tokio::io::duplex(4096);
        let (mut plugin_output, host_stdout) = tokio::io::duplex(4096);
        let http = Arc::new(FixtureProviderHttp::default());
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
        let task = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request_cancellable(
                        method,
                        json!({"alias_prefix":"fixture/"}),
                        &CancellationToken::default(),
                    )
                    .await
            }
        });
        let mut input = BufReader::new(plugin_input);
        let mut line = String::new();
        input.read_line(&mut line).await.expect("host request");
        let RpcFrame::Request(invocation) = serde_json::from_str(&line).expect("frame") else {
            panic!("host request expected")
        };
        let id = if wrong_id {
            RpcId::Number(9999)
        } else {
            invocation.id
        };
        let frame = json!({"jsonrpc":"2.0", "id":"forged-http", "method":METHOD_PROVIDER_HTTP,
            "params":{"invocation_id":id, "alias":alias, "credential_reference":"fixture-token",
                "request":{"url":"https://example.test", "method":"GET", "credential_header":"Authorization"}}});
        plugin_output
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .expect("forged request");
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("protocol rejection deadline")
            .expect("host caller");
        assert!(result.is_err());
        assert!(http.requests.lock().expect("request log").is_empty());
        client
            .settle_effects()
            .await
            .expect("rejected request has no HTTP effects");
    }
}
