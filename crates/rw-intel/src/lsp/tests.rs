#![allow(clippy::expect_used)]
use super::*;
use tempfile::tempdir;
use tokio::io::duplex;

struct UnavailableSpawner;

#[async_trait]
impl LspProcessSpawner for UnavailableSpawner {
    async fn spawn(
        &self,
        _workspace: &Path,
        _server: &LspServerConfig,
    ) -> Result<SpawnedLspProcess, LspError> {
        Err(LspError::Unavailable)
    }
}

struct NoopHandle;

#[async_trait]
impl LspProcessHandle for NoopHandle {
    async fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PushDiagnosticsSpawner;

#[async_trait]
impl LspProcessSpawner for PushDiagnosticsSpawner {
    async fn spawn(
        &self,
        _workspace: &Path,
        _server: &LspServerConfig,
    ) -> Result<SpawnedLspProcess, LspError> {
        let (client_stdin, server_stdin) = duplex(64 * 1024);
        let (server_stdout, client_stdout) = duplex(64 * 1024);
        tokio::spawn(fake_push_diagnostics_server(server_stdin, server_stdout));
        Ok(SpawnedLspProcess {
            handle: Box::new(NoopHandle),
            stdin: Box::pin(client_stdin),
            stdout: Box::pin(client_stdout),
        })
    }
}

struct CrossRootSpawner {
    target_uri: String,
}

#[async_trait]
impl LspProcessSpawner for CrossRootSpawner {
    async fn spawn(
        &self,
        _workspace: &Path,
        _server: &LspServerConfig,
    ) -> Result<SpawnedLspProcess, LspError> {
        let (client_stdin, server_stdin) = duplex(64 * 1024);
        let (server_stdout, client_stdout) = duplex(64 * 1024);
        tokio::spawn(fake_cross_root_server(
            server_stdin,
            server_stdout,
            self.target_uri.clone(),
        ));
        Ok(SpawnedLspProcess {
            handle: Box::new(NoopHandle),
            stdin: Box::pin(client_stdin),
            stdout: Box::pin(client_stdout),
        })
    }
}

async fn fake_cross_root_server(
    stdin: tokio::io::DuplexStream,
    mut stdout: tokio::io::DuplexStream,
    target_uri: String,
) {
    let mut stdin = BufReader::new(stdin);
    let initialize = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
        .await
        .expect("initialize");
    write_message(
        &mut stdout,
        &json!({"jsonrpc":"2.0", "id":initialize["id"], "result":{"capabilities":{}}}),
        DEFAULT_MAX_MESSAGE_BYTES,
    )
    .await
    .expect("initialize response");
    let _initialized = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
        .await
        .expect("initialized");
    for _ in 0..3 {
        let request = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
            .await
            .expect("request");
        let result = match request["method"].as_str() {
            Some("textDocument/definition") => json!({
                "uri":target_uri,
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}
            }),
            Some("textDocument/references") => json!([{
                "uri":target_uri,
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}
            }]),
            Some("textDocument/rename") => json!({"changes":{
                target_uri.clone(): [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"Cat"}]
            }}),
            _ => Value::Null,
        };
        write_message(
            &mut stdout,
            &json!({"jsonrpc":"2.0", "id":request["id"], "result":result}),
            DEFAULT_MAX_MESSAGE_BYTES,
        )
        .await
        .expect("response");
    }
}

async fn fake_push_diagnostics_server(
    stdin: tokio::io::DuplexStream,
    mut stdout: tokio::io::DuplexStream,
) {
    let mut stdin = BufReader::new(stdin);
    let initialize = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
        .await
        .expect("initialize");
    write_message(
        &mut stdout,
        &json!({"jsonrpc":"2.0", "id":initialize["id"], "result":{"capabilities":{}}}),
        DEFAULT_MAX_MESSAGE_BYTES,
    )
    .await
    .expect("initialize response");
    let _initialized = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
        .await
        .expect("initialized");
    for revision in 0..2 {
        let update = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
            .await
            .expect("document update");
        let uri = update["params"]["textDocument"]["uri"]
            .as_str()
            .expect("document URI")
            .to_owned();
        let pull = read_message(&mut stdin, DEFAULT_MAX_MESSAGE_BYTES)
            .await
            .expect("diagnostic pull");
        write_message(
            &mut stdout,
            &json!({"jsonrpc":"2.0", "id":pull["id"], "error":{"code":-32601,"message":"pull unsupported"}}),
            DEFAULT_MAX_MESSAGE_BYTES,
        )
        .await
        .expect("pull rejection");
        if revision == 1 {
            tokio::time::sleep(Duration::from_millis(350)).await;
            write_message(
                &mut stdout,
                &json!({"jsonrpc":"2.0", "method":"textDocument/publishDiagnostics", "params":{"uri":uri.replace("lib.rs", "other.rs"),"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"severity":2,"message":"unrelated diagnostic"}]}}),
                DEFAULT_MAX_MESSAGE_BYTES,
            )
            .await
            .expect("publish unrelated diagnostics");
            tokio::time::sleep(Duration::from_millis(350)).await;
            write_message(
                &mut stdout,
                &json!({"jsonrpc":"2.0", "method":"textDocument/publishDiagnostics", "params":{"uri":uri,"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"severity":1,"message":"push-only diagnostic"}]}}),
                DEFAULT_MAX_MESSAGE_BYTES,
            )
            .await
            .expect("publish diagnostics");
        }
    }
}

#[tokio::test]
async fn framing_accepts_unknown_headers_and_exact_body() {
    let (mut write, read) = duplex(1024);
    let mut read = BufReader::new(read);
    write.write_all(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 24\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1}").await.expect("write");
    let value = read_message(&mut read, 1024).await.expect("message");
    assert_eq!(value["id"], 1);
}

#[test]
fn document_version_and_diagnostic_caches_stay_bounded() {
    let mut versions = BTreeMap::new();
    for key in ["c", "b", "a"] {
        let key = key.to_owned();
        bound_map_for_insert(&mut versions, &key, 2);
        versions.insert(key, 1);
    }
    assert_eq!(versions.len(), 2);

    let mut diagnostics = BTreeMap::new();
    for key in ["c.rs", "b.rs", "a.rs"] {
        let key = PathBuf::from(key);
        bound_map_for_insert(&mut diagnostics, &key, 2);
        diagnostics.insert(key, Vec::<Diagnostic>::new());
    }
    assert_eq!(diagnostics.len(), 2);
}

#[test]
fn intelligence_layer_has_no_ambient_process_spawn_path() {
    let source = include_str!("../lsp.rs");
    let production = source.split("#[cfg(test)]").next().unwrap_or(source);
    assert!(!production.contains("tokio::process::Command"));
    assert!(!production.contains("std::process::Command"));
    assert!(!production.contains(".spawn()?"));
}

#[tokio::test]
async fn framing_rejects_duplicate_or_oversized_lengths() {
    for bytes in [
        b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
        b"Content-Length: 999\r\n\r\n".as_slice(),
    ] {
        let (mut write, read) = duplex(1024);
        let mut read = BufReader::new(read);
        write.write_all(bytes).await.expect("write");
        assert!(read_message(&mut read, 16).await.is_err());
    }
}

#[tokio::test]
async fn framing_rejects_unterminated_headers_at_the_cap() {
    let (mut write, read) = duplex(MAX_HEADER_BYTES * 2);
    let mut read = BufReader::new(read);
    write
        .write_all(&vec![b'x'; MAX_HEADER_BYTES + 1])
        .await
        .expect("write");
    assert!(matches!(
        read_message(&mut read, 16).await,
        Err(LspError::Protocol("header exceeds size limit"))
    ));
}

#[tokio::test]
async fn absent_server_degrades_definition_to_syntax_index() {
    let root = tempdir().expect("root");
    let syntax = Arc::new(SymbolIndex::new(root.path()).expect("index"));
    syntax
        .update_source("lib.rs", "pub struct Dog;\nfn f(_: Dog) {}\n")
        .expect("source");
    let intel = CodeIntelligence::new(
        root.path(),
        syntax,
        LspConfig {
            servers: Vec::new(),
            ..LspConfig::default()
        },
        Arc::new(UnavailableSpawner),
    )
    .expect("intel");
    let result = intel
        .definition(
            "lib.rs",
            Position {
                line: 1,
                character: 8,
            },
        )
        .await;
    assert_eq!(result.backend, IntelligenceBackend::TreeSitter);
    assert!(!result.items.is_empty());
}

#[tokio::test]
async fn push_only_diagnostics_arrive_in_same_turn_after_did_change() {
    let root = tempdir().expect("root");
    std::fs::write(root.path().join("lib.rs"), "fn first() {}\n").expect("source");
    std::fs::write(root.path().join("other.rs"), "fn other() {}\n").expect("other source");
    let syntax = Arc::new(SymbolIndex::new(root.path()).expect("index"));
    let intel = CodeIntelligence::new(
        root.path(),
        syntax,
        LspConfig {
            servers: vec![LspServerConfig {
                language: Language::Rust,
                command: PathBuf::from("fake-rust-analyzer"),
                args: Vec::new(),
            }],
            request_timeout: Duration::from_secs(1),
            notification_drain_timeout: Duration::from_secs(3),
            ..LspConfig::default()
        },
        Arc::new(PushDiagnosticsSpawner),
    )
    .expect("intel");
    let first = intel.diagnostics("lib.rs", "fn first() {}\n").await;
    assert!(first.items.is_empty());
    let changed = intel.diagnostics("lib.rs", "fn changed() {}\n").await;
    assert_eq!(changed.backend, IntelligenceBackend::Lsp);
    assert_eq!(changed.items.len(), 1);
    assert_eq!(changed.items[0].message, "push-only diagnostic");
}

#[tokio::test]
async fn two_root_server_results_are_retained_and_virtualized() {
    let first = tempdir().expect("first root");
    let second = tempdir().expect("second root");
    std::fs::write(first.path().join("lib.rs"), "fn use_dog() {}\n").expect("first source");
    std::fs::write(second.path().join("other.rs"), "struct Dog;\n").expect("second source");
    let roots = vec![first.path().to_path_buf(), second.path().to_path_buf()];
    let mapper = Arc::new(WorkspaceUriMapper::new(&roots).expect("URI mapper"));
    let target_uri = Url::from_file_path(second.path().join("other.rs"))
        .expect("target URI")
        .to_string();
    let intel = CodeIntelligence::new_with_uri_mapper(
        first.path(),
        Arc::new(SymbolIndex::new(first.path()).expect("index")),
        LspConfig {
            servers: vec![LspServerConfig {
                language: Language::Rust,
                command: PathBuf::from("fake-rust-analyzer"),
                args: Vec::new(),
            }],
            ..LspConfig::default()
        },
        Arc::new(CrossRootSpawner { target_uri }),
        mapper,
    )
    .expect("intel");
    let position = Position {
        line: 0,
        character: 3,
    };
    let definition = intel.definition("lib.rs", position).await;
    let references = intel.references("lib.rs", position).await;
    let rename = intel.rename("lib.rs", position, "Cat").await;
    let expected = Path::new("@root/1/other.rs");
    assert_eq!(definition.items[0].path, expected);
    assert_eq!(references.items[0].path, expected);
    assert_eq!(rename.edits[0].path, expected);
}

#[test]
fn workspace_edits_drop_outside_uris_and_bound_text() {
    let root = tempdir().expect("root");
    std::fs::write(root.path().join("lib.rs"), "Dog").expect("source");
    let mapper = WorkspaceUriMapper::new(&[root.path().to_path_buf()]).expect("URI mapper");
    let inside = Url::from_file_path(root.path().join("lib.rs")).expect("uri");
    let edits = parse_workspace_edit(
        &mapper,
        &json!({"changes": {inside.as_str(): [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"Dog"}], "file:///tmp/outside.rs":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"Bad"}]}}),
        10,
    );
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].path, PathBuf::from("lib.rs"));
}
