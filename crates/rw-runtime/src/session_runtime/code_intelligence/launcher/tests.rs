#![allow(clippy::expect_used)]
use super::*;
use rw_tools::{CodeIntelligence, IntelligenceBackend, LspConfig, Position, SymbolIndex};

#[tokio::test]
async fn syntax_queries_do_not_prepare_native_launch_authority() {
    let root = tempfile::tempdir().expect("workspace");
    let syntax = Arc::new(SymbolIndex::new(root.path()).expect("index"));
    syntax
        .update_source("lib.rs", "pub struct Dog;\nfn f(_: Dog) {}\n")
        .expect("source");
    let spawner = Arc::new(DeferredLspSpawner::new(&[root.path().to_path_buf()]));
    let intel = CodeIntelligence::new(
        root.path(),
        syntax,
        LspConfig {
            servers: Vec::new(),
            ..LspConfig::default()
        },
        spawner.clone(),
    )
    .expect("intelligence");
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
    assert!(spawner.prepared.try_lock().expect("preparation").is_none());
}

struct PendingKill {
    started: Option<tokio::sync::oneshot::Sender<()>>,
    finish: tokio::sync::oneshot::Receiver<()>,
}
#[async_trait]
impl LspProcessHandle for PendingKill {
    fn request_termination(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn kill(&mut self) -> io::Result<()> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        (&mut self.finish).await.map_err(io::Error::other)
    }
}
#[tokio::test]
async fn dropped_lsp_handle_retains_scratch_until_actual_settlement() {
    let root = tempfile::tempdir().expect("workspace");
    let owner = prepare(&[root.path().to_path_buf()], &mut None).expect("prepare");
    let weak = Arc::downgrade(&owner);
    let scratch = owner.scratch.path().to_path_buf();
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, finish) = tokio::sync::oneshot::channel();
    drop(OwnedLspHandle(Some(PhysicalLsp {
        handle: Box::new(PendingKill {
            started: Some(started),
            finish,
        }),
        owner,
    })));
    entered.await.expect("physical cleanup entered");
    assert!(scratch.is_dir());
    assert!(weak.upgrade().is_some());
    release.send(()).expect("settle process");
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("release proven owner");
    assert!(!scratch.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn first_native_launch_prepares_once_and_preserves_protocol_and_cleanup() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let root = tempfile::tempdir().expect("workspace");
    let spawner = DeferredLspSpawner::new(&[root.path().to_path_buf()]);
    let server = LspServerConfig {
        language: rw_tools::Language::Rust,
        command: PathBuf::from("/bin/cat"),
        args: Vec::new(),
    };
    let (first, second) = tokio::join!(
        spawner.spawn(root.path(), &server),
        spawner.spawn(root.path(), &server),
    );
    let mut first = first.expect("first child");
    let mut second = second.expect("contending child");
    let owner = spawner
        .prepared
        .try_lock()
        .expect("prepared")
        .clone()
        .expect("owner");
    let scratch = owner.scratch.path().to_path_buf();
    assert!(Arc::ptr_eq(
        &owner,
        spawner
            .prepared
            .try_lock()
            .expect("prepared")
            .as_ref()
            .expect("owner")
    ));
    first.stdin.write_all(b"protocol\n").await.expect("input");
    let mut output = [0; 9];
    tokio::time::timeout(Duration::from_secs(2), first.stdout.read_exact(&mut output))
        .await
        .expect("protocol deadline")
        .expect("output");
    assert_eq!(&output, b"protocol\n");
    drop(spawner);
    drop(owner);
    first.handle.kill().await.expect("first process proof");
    assert!(scratch.is_dir(), "second child retains shared scratch");
    second.handle.kill().await.expect("second process proof");
    assert!(!scratch.exists());
}
