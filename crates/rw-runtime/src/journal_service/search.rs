//! The storage-root service owns one lazy search writer, shared across its session family.
use super::JournalService;
use miette::{IntoDiagnostic, Result, miette};
use rw_store::session::{SessionIndex, SessionStoreError, journal::JournalReadView};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(super) struct SearchIndex {
    root: PathBuf,
    writer: Mutex<Option<Arc<SessionIndex>>>,
}
impl SearchIndex {
    pub(super) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            writer: Mutex::new(None),
        }
    }
}
impl JournalService {
    pub(crate) fn search_index(
        &self,
        session: &str,
        source: &JournalReadView,
    ) -> Result<Arc<SessionIndex>> {
        self.root.validate_view(session, source).into_diagnostic()?;
        let _initialize =
            tracing::trace_span!(target: "rw_performance", "search.writer_owner").entered();
        let mut writer = self
            .search_index
            .writer
            .lock()
            .map_err(|_| miette!("search writer initialization failed"))?;
        if let Some(writer) = writer.as_ref() {
            return Ok(Arc::clone(writer));
        }
        let index = match SessionIndex::open(&self.search_index.root) {
            Ok(index) => index,
            Err(SessionStoreError::UnsupportedSqliteSchema {
                table: "sessions" | "search_documents" | "sessions_fts" | "search_invocations",
            }) => SessionIndex::reset_derived(&self.search_index.root).into_diagnostic()?,
            Err(error) => return Err(error).into_diagnostic(),
        };
        self.root.validate_view(session, source).into_diagnostic()?;
        let index = Arc::new(index);
        *writer = Some(Arc::clone(&index));
        Ok(index)
    }
}

#[cfg(test)]
mod tests;
