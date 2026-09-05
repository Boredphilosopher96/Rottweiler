use super::*;
use rw_ext::{ReadyPluginEndpoint, RpcToolAdapter};
use rw_tools::{DelegatedTools, ReadTool, Tool, ToolContext, ToolLimits, ToolRegistry, WriteTool};

fn manifest() -> PluginManifest {
    serde_json::from_value(json!({
        "name":"code-only-effects","version":"1.0.0","protocol":3,
        "capabilities":{"tools":[
            {"name":"native_probe","description":"Probe denied ambient effects","schema":{"type":"object"},"caps":["reads-fs","writes-fs","network","exec"]},
            {"name":"scoped_read","description":"Read through owned host scope","schema":{"type":"object"},"caps":["reads-fs"]},
            {"name":"scoped_write","description":"Write through owned host scope","schema":{"type":"object"},"caps":["reads-fs","writes-fs"]}
        ],"push":["effect/tool_call"]}
    })).expect("exact SDK manifest")
}

#[tokio::test]
async fn native_workers_deny_ambient_effects_and_execute_only_scoped_host_tools() {
    let _admission = crate::native_fixture::admit().await;
    let (bun, sdk) = bun_and_sdk();
    let scratch = tempfile::tempdir().expect("scratch");
    let workspace = tempfile::tempdir().expect("workspace");
    let package = workspace.path().join("plugin-code");
    std::fs::create_dir(&package).expect("package");
    let secret = workspace.path().join("secret");
    let output = workspace.path().join("output");
    std::fs::write(&secret, "workspace canary").expect("secret");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("network canary");
    listener.set_nonblocking(true).expect("nonblocking canary");
    let config = compiled_fixture_config(&bun, &sdk, &package, "code-only-effects.ts")
        .with_allowed_domains(["example.com"])
        .expect("approved host domains");
    let launcher = SandboxedPluginLauncher::new(
        scratch.path(),
        &helper_executable().expect("fixture helper"),
    )
    .expect("enforced sandbox");
    let manifest = manifest();
    let host = Arc::new(
        approved_production_host(
            &launcher,
            &config,
            workspace.path(),
            manifest.clone(),
            "code-only:effects",
        )
        .await,
    );
    let response = host.client().call_tool(rw_plugin_protocol::ToolCallParams {
        name:"native_probe".into(),
        input:json!({"secret":secret,"output":output,"url":format!("http://{}/",listener.local_addr().expect("address"))}),
        lifetime:rw_plugin_protocol::OperationLifetime::default(),
    }, &rw_tools::CancellationToken::default(),Arc::new(rw_tools::NoopProgressSink),None).await.expect("native probe");
    assert_eq!(
        response["data"],
        json!({"read":true,"write":true,"process":true,"network":true})
    );
    assert!(!output.exists());
    assert_eq!(
        listener.accept().expect_err("no ambient connection").kind(),
        std::io::ErrorKind::WouldBlock
    );
    let endpoint = Arc::new(ReadyPluginEndpoint::new(host.clone()).expect("endpoint"));
    let mut tools = ToolRegistry::default();
    tools
        .register(Arc::new(ReadTool::new(ToolLimits::default())))
        .expect("read registry");
    tools
        .register(Arc::new(WriteTool::new(ToolLimits::default())))
        .expect("write registry");
    let tools = Arc::new(tools);
    for (name, input, expected) in [
        (
            "scoped_read",
            json!({"path":"secret","line_count":null}),
            "workspace canary",
        ),
        (
            "scoped_write",
            json!({"path":"output","content":"owned mutation"}),
            "output",
        ),
    ] {
        let declaration = manifest
            .capabilities
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .expect("declaration");
        let adapter = RpcToolAdapter::new(declaration.clone(), endpoint.clone()).expect("adapter");
        let context = ToolContext::new(workspace.path()).expect("workspace context");
        let effects = Arc::new(DelegatedTools::new(
            context.clone(),
            tools.clone(),
            adapter.descriptor().capabilities,
            adapter.mutation_scope(&input),
        ));
        let context = context.with_effect_host(effects);
        let result = adapter
            .execute(&context, input)
            .await
            .expect("brokered tool result");
        assert!(result.content.contains(expected), "{}", result.content);
    }
    assert_eq!(
        std::fs::read_to_string(output).expect("owned write"),
        "owned mutation"
    );
    host.shutdown().await.expect("whole host proof");
}
