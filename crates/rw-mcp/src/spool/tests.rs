use super::*;
use crate::{CompactJsonEncoder, encoding};
use rw_store::session::journal::JournalRoot;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct LazySource {
    root: JournalRoot,
    store: Mutex<Option<Arc<SessionPayloadStore>>>,
    opened: AtomicUsize,
}
impl PayloadSource for LazySource {
    fn open(&self) -> io::Result<Arc<SessionPayloadStore>> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?;
        if let Some(store) = &*store {
            return Ok(Arc::clone(store));
        }
        let next = Arc::new(self.root.payloads("session").map_err(io::Error::other)?);
        self.opened.fetch_add(1, Ordering::AcqRel);
        *store = Some(Arc::clone(&next));
        Ok(next)
    }
}
struct Redactor;
impl PayloadRedactor for Redactor {
    fn redact(
        &self,
        text: &str,
        max_bytes: usize,
        admit: &mut dyn FnMut(usize) -> io::Result<()>,
    ) -> io::Result<String> {
        let size = text
            .len()
            .checked_add(text.matches("private-secret").count() * 3)
            .filter(|size| *size <= max_bytes)
            .ok_or_else(|| io::Error::other("redaction limit"))?;
        admit(size)?;
        Ok(text.replace("private-secret", "[safe-redaction]"))
    }
}
#[tokio::test]
async fn payload_is_redacted_before_durable_identity_and_retrievable_after_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let source = Arc::new(LazySource {
        root: JournalRoot::open(root.path())?,
        store: Mutex::new(None),
        opened: AtomicUsize::new(0),
    });
    let spool = FilesystemSpool::new(source.clone(), Arc::new(Redactor));
    assert!(!root.path().join("sessions/session/payloads").exists());
    let encoded = encoding::encode(
        Arc::new(CompactJsonEncoder),
        crate::McpResponseSlot::new(crate::McpResponseLimits::new(64 * 1024)?)?.adopt(serde_json::json!({"value":"private-secret"})).await?,
    )
    .await?;
    let server = McpServerId::new("fixture")?;
    let reference = spool.write(&server, "tool", encoded).await?;
    let first = spool
        .window(reference.clone(), 0, None, CancellationToken::default())
        .await?;
    assert!(first.window.content.contains("[safe-redaction]"));
    assert!(!first.window.content.contains("private-secret"));
    assert_eq!(reference.bytes, first.window.content.len());
    assert_eq!(first.payloads.references(), vec![reference.clone()]);
    assert_eq!(source.opened.load(Ordering::Acquire), 1);
    drop(first);
    spool.settle_effects().await?;
    drop(spool);
    drop(source);
    let reopened = JournalRoot::open(root.path())?.payloads("session")?;
    let next = reopened.window(&reference, 0, None, &|| false)?;
    assert!(next.content.contains("[safe-redaction]"));
    assert!(!next.content.contains("private-secret"));
    Ok(())
}
