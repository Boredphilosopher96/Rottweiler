#![cfg(test)]
use super::Arc;
use super::AtomicUsize;
use super::BTreeMap;
use super::BuildToolsInput;
use super::CommandFixtureMode;
use super::CommandSafetyClassifier;
use super::Config;
use super::DeferredCredentialResolver;
use super::DeferredToolProxy;
use super::DeferredWebSearchHeaders;
use super::ExecutionLease;
use super::FixtureRedactor;
use super::FolderTrustStore;
use super::HeadlessQuestionAsker;
use super::IntelligenceBackend;
use super::MultiRootCodeIntelligence;
use super::Ordering;
use super::Path;
use super::PathBuf;
use super::Position;
use super::SharedCommandFixtureRedactor;
use super::WebSearchConfig;
use super::WorkspaceSymbolIndex;
use super::build_tools;
use super::command_mode_can_open_proxy;
use super::lsp_servers_for_root;
use super::miette;
use super::resolve_tool_proxy;
use super::resolve_websearch_headers_with;
use super::tempdir;
use super::trusted_lsp_roots;
use rw_tools::CodeIntelligenceProvider;

#[test]
fn replay_and_offline_command_modes_never_enable_command_egress() {
    assert!(!command_mode_can_open_proxy(&CommandFixtureMode::Replay {
        directory: PathBuf::from("fixtures"),
    }));
    assert!(!command_mode_can_open_proxy(&CommandFixtureMode::Offline));
    assert!(command_mode_can_open_proxy(&CommandFixtureMode::Live));
}

#[test]
#[allow(clippy::too_many_lines)]
fn build_tools_registers_intelligence_and_only_configured_live_websearch() {
    let root = tempdir().expect("workspace");
    let private = tempdir().expect("private");
    let lease = Arc::new(
        ExecutionLease::acquire(private.path().join("execution.lock")).expect("execution lease"),
    );
    let configured = WebSearchConfig {
        endpoint: Some("https://search.example/v1".to_owned()),
        query_parameter: "query".to_owned(),
        header_credentials: BTreeMap::new(),
    };
    let built = build_tools(BuildToolsInput {
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        workspace_roots: &[root.path().to_path_buf()],
        trusted_lsp_roots: &[false],
        question_asker: Arc::new(HeadlessQuestionAsker),
        offline: false,
        global_proxy: None,
        deferred_global_proxy: None,
        command_fixture_mode: CommandFixtureMode::Offline,
        execution_lease: lease,
        command_safety: &Arc::new(CommandSafetyClassifier::default()),
        websearch_config: &configured,
        websearch_headers: &BTreeMap::new(),
        deferred_websearch_headers: None,
        native_websearch_possible: false,
        background_redactor: Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
        background_manager: None,
    })
    .expect("tool composition");
    for name in [
        "background_status",
        "background_output",
        "background_kill",
        "diagnostics",
        "definition",
        "references",
        "rename",
        "submit_plan",
        "websearch",
    ] {
        assert!(built.registry.resolve(name).is_some(), "missing {name}");
    }
    assert!(
        built
            .registry
            .descriptor("bash")
            .and_then(|descriptor| descriptor
                .input_schema
                .pointer("/properties/run_in_background"))
            .is_some(),
        "bash schema must expose typed background execution"
    );

    let offline_lease = Arc::new(
        ExecutionLease::acquire(private.path().join("offline-execution.lock"))
            .expect("offline execution lease"),
    );
    let offline = build_tools(BuildToolsInput {
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        workspace_roots: &[root.path().to_path_buf()],
        trusted_lsp_roots: &[false],
        question_asker: Arc::new(HeadlessQuestionAsker),
        offline: true,
        global_proxy: None,
        deferred_global_proxy: None,
        command_fixture_mode: CommandFixtureMode::Offline,
        execution_lease: offline_lease,
        command_safety: &Arc::new(CommandSafetyClassifier::default()),
        websearch_config: &configured,
        websearch_headers: &BTreeMap::new(),
        deferred_websearch_headers: None,
        native_websearch_possible: false,
        background_redactor: Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
        background_manager: None,
    })
    .expect("offline tool composition");
    assert!(offline.registry.resolve("websearch").is_none());
    assert!(offline.registry.resolve("definition").is_some());

    let replay_lease = Arc::new(
        ExecutionLease::acquire(private.path().join("replay-execution.lock"))
            .expect("replay execution lease"),
    );
    let replay_native = build_tools(BuildToolsInput {
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        workspace_roots: &[root.path().to_path_buf()],
        trusted_lsp_roots: &[false],
        question_asker: Arc::new(HeadlessQuestionAsker),
        offline: true,
        global_proxy: None,
        deferred_global_proxy: None,
        command_fixture_mode: CommandFixtureMode::Offline,
        execution_lease: replay_lease,
        command_safety: &Arc::new(CommandSafetyClassifier::default()),
        websearch_config: &configured,
        websearch_headers: &BTreeMap::new(),
        deferred_websearch_headers: None,
        native_websearch_possible: true,
        background_redactor: Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
        background_manager: None,
    })
    .expect("native replay tool composition");
    assert!(replay_native.registry.resolve("websearch").is_some());
}

#[test]
fn untrusted_root_removes_lsp_server_before_any_spawn_boundary() {
    let server = rw_tools::LspServerConfig {
        language: rw_tools::Language::Rust,
        command: PathBuf::from("/trusted/outside/rust-analyzer"),
        args: Vec::new(),
    };
    assert!(lsp_servers_for_root(std::slice::from_ref(&server), false).is_empty());
    assert_eq!(
        lsp_servers_for_root(std::slice::from_ref(&server), true),
        vec![server]
    );
}

#[test]
fn lsp_trust_is_assessed_independently_for_added_roots() {
    let first = tempdir().expect("first root");
    let added = tempdir().expect("added root");
    let private = tempdir().expect("private");
    let ledger = private.path().join("trust.json");
    let store = FolderTrustStore::new(ledger.clone());
    let first_assessment = store.assess(first.path()).expect("first assessment");
    store.grant(&first_assessment).expect("trust first");
    let states = trusted_lsp_roots(
        &[first.path().to_path_buf(), added.path().to_path_buf()],
        &ledger,
        false,
    )
    .expect("trust states");
    assert_eq!(states, [true, false]);
}

#[tokio::test]
async fn multi_root_intelligence_routes_and_virtualizes_tree_sitter_fallback() {
    let primary = tempdir().expect("primary");
    let added = tempdir().expect("added");
    std::fs::write(primary.path().join("lib.rs"), "pub struct Primary;\n").expect("primary source");
    std::fs::write(
        added.path().join("lib.rs"),
        "pub struct Added;\nfn use_it(_: Added) {}\n",
    )
    .expect("added source");
    let symbols =
        Arc::new(WorkspaceSymbolIndex::new([primary.path(), added.path()]).expect("symbols"));
    let intelligence = MultiRootCodeIntelligence::new(
        &[primary.path().to_path_buf(), added.path().to_path_buf()],
        &[false, false],
        symbols,
        true,
    )
    .expect("intelligence");
    let result = intelligence
        .definition(
            Path::new("@root/1/lib.rs"),
            Position {
                line: 1,
                character: 13,
            },
        )
        .await;
    assert_eq!(result.backend, IntelligenceBackend::TreeSitter);
    assert!(
        result
            .items
            .iter()
            .any(|location| location.path == Path::new("@root/1/lib.rs"))
    );
}

#[test]
fn offline_tool_proxy_resolution_never_touches_credentials() {
    let mut config = Config::default();
    config.network.proxy = Some("http://127.0.0.1:9".to_owned());
    config.network.proxy_username = Some("user".to_owned());
    config.network.proxy_password_credential = Some("missing-secret".to_owned());
    let missing = PathBuf::from("/definitely/missing/credentials.toml");
    assert!(
        resolve_tool_proxy(&config, &missing, true, &FixtureRedactor::default())
            .expect("offline resolution")
            .is_none()
    );
}

#[test]
fn websearch_credentials_are_skipped_offline_and_registered_for_redaction() {
    let config = WebSearchConfig {
        endpoint: Some("https://search.example/v1".to_owned()),
        query_parameter: "q".to_owned(),
        header_credentials: BTreeMap::from([(
            "Authorization".to_owned(),
            "search-api-token".to_owned(),
        )]),
    };
    let redactor = FixtureRedactor::default();
    let calls = std::cell::Cell::new(0_u8);
    let offline = resolve_websearch_headers_with(&config, true, &redactor, |_| {
        calls.set(calls.get().saturating_add(1));
        Err(miette!("credential boundary must not run offline"))
    })
    .expect("offline search credentials");
    assert!(offline.is_empty());
    assert_eq!(calls.get(), 0);

    let canary = "Bearer websearch-secret-canary";
    let online = resolve_websearch_headers_with(&config, false, &redactor, |_| {
        calls.set(calls.get().saturating_add(1));
        Ok(canary.to_owned())
    })
    .expect("online search credentials");
    assert_eq!(
        online.get("Authorization").map(String::as_str),
        Some(canary)
    );
    assert_eq!(calls.get(), 1);
    assert!(!redactor.redact_text(canary).contains(canary));
    assert!(!format!("{config:?}").contains(canary));
}

#[tokio::test]
async fn tool_composition_defers_all_external_credential_backend_reads() {
    let root = tempdir().expect("workspace");
    let private = tempdir().expect("private state");
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver_calls = Arc::clone(&calls);
    let resolver: DeferredCredentialResolver = Arc::new(move |reference| {
        resolver_calls.fetch_add(1, Ordering::SeqCst);
        match reference {
            "proxy-password" => Ok("proxy-secret-canary".to_owned()),
            "search-token" => Ok("Bearer search-secret-canary".to_owned()),
            _ => Err("unexpected credential reference".to_owned()),
        }
    });
    let redactor = FixtureRedactor::default();
    let deferred_proxy = DeferredToolProxy::with_resolver(
        "http://127.0.0.1:9",
        Some("proxy-user".to_owned()),
        Some("proxy-password".to_owned()),
        redactor.clone(),
        Arc::clone(&resolver),
    );
    let websearch_config = WebSearchConfig {
        endpoint: Some("https://search.example/v1".to_owned()),
        query_parameter: "q".to_owned(),
        header_credentials: BTreeMap::from([(
            "Authorization".to_owned(),
            "search-token".to_owned(),
        )]),
    };
    let deferred_headers = DeferredWebSearchHeaders::with_resolver(
        websearch_config.clone(),
        redactor.clone(),
        resolver,
    );
    let lease = Arc::new(
        ExecutionLease::acquire(private.path().join("execution.lock")).expect("execution lease"),
    );

    let built = build_tools(BuildToolsInput {
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        workspace_roots: &[root.path().to_path_buf()],
        trusted_lsp_roots: &[false],
        question_asker: Arc::new(HeadlessQuestionAsker),
        offline: false,
        global_proxy: None,
        deferred_global_proxy: Some(deferred_proxy.clone()),
        command_fixture_mode: CommandFixtureMode::Offline,
        execution_lease: lease,
        command_safety: &Arc::new(CommandSafetyClassifier::default()),
        websearch_config: &websearch_config,
        websearch_headers: &BTreeMap::new(),
        deferred_websearch_headers: Some(deferred_headers.clone()),
        native_websearch_possible: false,
        background_redactor: Arc::new(SharedCommandFixtureRedactor(redactor.clone())),
        background_manager: None,
    })
    .expect("tool composition");
    assert!(built.registry.resolve("webfetch").is_some());
    assert!(built.registry.resolve("websearch").is_some());
    assert!(built.registry.resolve("bash").is_some());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "ordinary startup must not read the credential backend"
    );

    deferred_proxy
        .resolve()
        .await
        .expect("explicit proxy-backed operation resolves credentials");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let headers = deferred_headers
        .resolve()
        .await
        .expect("explicit search operation resolves credentials");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        headers.get("Authorization").map(String::as_str),
        Some("Bearer search-secret-canary")
    );
    assert!(
        !redactor
            .redact_text("proxy-secret-canary")
            .contains("proxy-secret-canary")
    );
    assert!(
        !redactor
            .redact_text("Bearer search-secret-canary")
            .contains("search-secret-canary")
    );
}
