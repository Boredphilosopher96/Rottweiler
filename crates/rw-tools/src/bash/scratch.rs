//! Shared private directory authority survives every native command effect.
use std::{io, path::Path, sync::Arc};

/// A private command workspace removed only after its final physical owner ends.
/// Sandboxed executors and their in-flight process states share this exact owner.
#[derive(Debug)]
pub struct CommandScratch {
    directory: tempfile::TempDir,
}
impl CommandScratch {
    /// Create a uniquely named private directory for a command execution domain.
    ///
    /// # Errors
    /// Rejects an invalid domain name or a failed private directory creation.
    pub fn create(kind: &str) -> io::Result<Arc<Self>> {
        if kind.is_empty()
            || kind.len() > 64
            || !kind
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid scratch domain",
            ));
        }
        let directory = tempfile::Builder::new()
            .prefix(&format!("rottweiler-{kind}-"))
            .tempdir()?;
        Ok(Arc::new(Self { directory }))
    }

    /// The exact private directory governed by this owner.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}
