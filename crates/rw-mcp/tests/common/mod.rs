use rw_mcp::{FilesystemSpool, PayloadRedactor};
use rw_store::session::journal::JournalRoot;
use std::{io, path::Path, sync::Arc};

struct FixtureRedactor;
impl PayloadRedactor for FixtureRedactor {
    fn redact(
        &self,
        text: &str,
        max_bytes: usize,
        admit: &mut dyn FnMut(usize) -> io::Result<()>,
    ) -> io::Result<String> {
        if text.len() > max_bytes {
            return Err(io::Error::other("fixture payload oversized"));
        }
        admit(text.len())?;
        Ok(text.to_owned())
    }
}
pub fn spool(root: &Path) -> Arc<FilesystemSpool> {
    let journal = JournalRoot::open(root).expect("fixture journal root");
    let source = journal.payloads("fixture").expect("fixture payload owner");
    Arc::new(FilesystemSpool::new(
        Arc::new(source),
        Arc::new(FixtureRedactor),
    ))
}
