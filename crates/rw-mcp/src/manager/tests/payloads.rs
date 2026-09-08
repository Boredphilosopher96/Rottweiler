use super::*;
use crate::{FilesystemSpool, McpResponseUse, PayloadRedactor};
use std::io;
struct Redactor;
impl PayloadRedactor for Redactor {
    fn redact(
        &self,
        text: &str,
        max_bytes: usize,
        admit: &mut dyn FnMut(usize) -> io::Result<()>,
    ) -> io::Result<String> {
        if text.len() > max_bytes {
            return Err(io::Error::other("payload too large"));
        }
        admit(text.len())?;
        Ok(text.to_owned())
    }
}
#[tokio::test]
async fn inline_prompt_rejects_oversize_before_publishing_any_payload()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let store = rw_store::session::journal::JournalRoot::open(root.path())?.payloads("session")?;
    let directory = root.path().join("sessions/session/payloads");
    let before = std::fs::read_dir(&directory)?.count();
    let manager = McpManager::new(
        Arc::new(MockConnector {
            clients: Mutex::new(BTreeMap::new()),
        }),
        Arc::new(FilesystemSpool::new(Arc::new(store), Arc::new(Redactor))),
        Arc::new(CompactJsonEncoder),
        McpLimits::default(),
    );
    let server = McpServerId::new("fixture")?;
    let use_case = McpResponseUse::Inline {
        max_bytes: 32 * 1024,
    };
    assert!(
        manager
            .cap(
                &server,
                "prompt",
                crate::McpResponseSlot::new(crate::McpResponseLimits::new(512 * 1024)?)?
                    .adopt(json!({"body":"x".repeat(64 * 1024)}))
                    .await?,
                use_case
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(&directory)?.count(), before);
    let response = manager
        .cap(
            &server,
            "prompt",
            crate::McpResponseSlot::new(crate::McpResponseLimits::new(64 * 1024)?)?
                .adopt(json!({"body":"small"}))
                .await?,
            use_case,
        )
        .await?;
    assert_eq!(response.encoded, r#"{"body":"small"}"#);
    assert!(response.overflow.is_none());
    assert!(response.payloads.references().is_empty());
    assert_eq!(std::fs::read_dir(&directory)?.count(), before);
    Ok(())
}
