//! Canonical document bodies are built once and served through borrowed UTF-8 chunks.

use super::page;
use rw_core::{HostError, transcript::TranscriptDocument};
use rw_store::session::{journal::JournalReadView, transcript_index::TranscriptIndex};
use rw_types::{
    SessionId,
    transcript::{TranscriptContentPage, TranscriptContentRead},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_DOCUMENTS: usize = 32;

struct CachedDocument {
    document: Arc<TranscriptDocument>,
    bytes: usize,
    touched: u64,
}
#[derive(Default)]
pub(super) struct DocumentCache {
    entries: HashMap<String, CachedDocument>,
    bytes: usize,
    clock: u64,
    builds: u64,
}
impl DocumentCache {
    #[cfg(test)]
    pub(super) fn build_count(&self) -> u64 {
        self.builds
    }

    fn get(&mut self, key: &str) -> Option<Arc<TranscriptDocument>> {
        let entry = self.entries.get_mut(key)?;
        self.clock = self.clock.wrapping_add(1);
        entry.touched = self.clock;
        Some(Arc::clone(&entry.document))
    }
    fn insert(
        &mut self,
        key: String,
        document: TranscriptDocument,
    ) -> Result<Arc<TranscriptDocument>, HostError> {
        let bytes = document
            .retained_bytes()
            .saturating_add(key.capacity())
            .saturating_add(128);
        if bytes > MAX_DOCUMENT_BYTES {
            return Err(HostError::Query(
                "document exceeds the retained content budget".into(),
            ));
        }
        while self.entries.len() >= MAX_DOCUMENTS
            || self.bytes.saturating_add(bytes) > MAX_DOCUMENT_BYTES
        {
            let victim = self
                .entries
                .iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.document) == 1)
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
                .ok_or_else(|| {
                    HostError::Query("document content admission is exhausted".into())
                })?;
            if let Some(removed) = self.entries.remove(&victim) {
                self.bytes -= removed.bytes;
            }
        }
        self.clock = self.clock.wrapping_add(1);
        self.builds = self.builds.saturating_add(1);
        let document = Arc::new(document);
        self.entries.insert(
            key,
            CachedDocument {
                document: Arc::clone(&document),
                bytes,
                touched: self.clock,
            },
        );
        self.bytes += bytes;
        Ok(document)
    }
}

pub(super) fn validate(
    session: &SessionId,
    request: &TranscriptContentRead,
) -> Result<(), HostError> {
    if &request.view.session_id != session
        || request.view.projection_version != rw_types::transcript::TRANSCRIPT_PROJECTION_VERSION
    {
        return Err(HostError::Protocol(
            "content view identity is invalid".into(),
        ));
    }
    if !(4..=64 * 1024).contains(&request.max_bytes)
        || request
            .view
            .through
            .is_none_or(|through| request.source.sequence > through)
    {
        return Err(HostError::Protocol(
            "content range is outside its view or byte limit".into(),
        ));
    }
    Ok(())
}

pub(super) fn read(
    cache: &Mutex<DocumentCache>,
    index: &TranscriptIndex,
    journal: &JournalReadView,
    request: &TranscriptContentRead,
) -> Result<TranscriptContentPage, HostError> {
    let head = index.head().map_err(page::storage)?;
    if head.generation != request.view.generation.0 {
        return Err(HostError::Protocol(
            "transcript ordering changed; reload the content source".into(),
        ));
    }
    let key = serde_json::to_string(&(&request.view, &request.source)).map_err(page::storage)?;
    let cached = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key);
    let document = if let Some(document) = cached {
        document
    } else {
        let pinned = journal
            .at_prefix(page::prefix(&request.view)?)
            .map_err(page::storage)?;
        if request.source.sequence.0 >= pinned.prefix_identity().next_sequence {
            return Err(HostError::Protocol(
                "content source exceeds requested view".into(),
            ));
        }
        let event = rw_core::transcript::TranscriptProjector::materialize_source(
            index,
            journal,
            request.source.sequence,
        )
        .map_err(page::storage)?;
        let document = TranscriptDocument::from_event(event, &request.source, MAX_DOCUMENT_BYTES)
            .map_err(page::storage)?;
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, document)?
    };
    let chunk = document
        .chunk(request.offset as usize, request.max_bytes as usize)
        .map_err(page::storage)?;
    Ok(TranscriptContentPage {
        view: request.view.clone(),
        source: request.source.clone(),
        offset: request.offset,
        next_offset: chunk
            .next_offset
            .map(u32::try_from)
            .transpose()
            .map_err(page::storage)?,
        total_bytes: u32::try_from(document.total_bytes()).map_err(page::storage)?,
        format: document.format(),
        text: chunk.text.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn document(bytes: usize) -> TranscriptDocument {
        let event = rw_types::EngineEvent::ConversationTurnCommitted {
            meta: rw_types::EventMeta {
                protocol_version: rw_types::PROTOCOL_VERSION,
                session_id: SessionId("document".into()),
                sequence_id: rw_types::SequenceId(0),
                emitted_at: "2026-09-04T00:00:00Z".into(),
                caused_by: None,
            },
            agent_turn: 0,
            turn: rw_types::Turn {
                role: rw_types::Role::User,
                blocks: vec![rw_types::Block::Text {
                    text: "x".repeat(bytes),
                }],
                meta: rw_types::TurnMeta::default(),
            },
        };
        TranscriptDocument::from_event(
            event,
            &rw_types::transcript::TranscriptContentSource {
                sequence: rw_types::SequenceId(0),
                selector: rw_types::transcript::TranscriptContentSelector::ConversationBlock {
                    index: 0,
                },
            },
            MAX_DOCUMENT_BYTES,
        )
        .expect("document")
    }

    #[test]
    fn aggregate_documents_keep_pinned_readers_charged_until_release() {
        let mut cache = DocumentCache::default();
        let first = cache
            .insert("first".into(), document(6 * 1024 * 1024))
            .expect("first");
        let second = cache
            .insert("second".into(), document(6 * 1024 * 1024))
            .expect("second");
        let original_bytes = cache.bytes;
        assert!(
            cache
                .insert("blocked".into(), document(6 * 1024 * 1024))
                .is_err()
        );
        assert_eq!(cache.bytes, original_bytes);
        let held_clone = Arc::clone(&first);
        drop(first);
        assert!(
            cache
                .insert("still-blocked".into(), document(6 * 1024 * 1024))
                .is_err()
        );
        drop(held_clone);
        let replacement = cache
            .insert("replacement".into(), document(6 * 1024 * 1024))
            .expect("eviction");
        assert!(cache.get("first").is_none());
        assert!(Arc::ptr_eq(
            &second,
            &cache.get("second").expect("pinned second")
        ));
        assert!(cache.bytes <= MAX_DOCUMENT_BYTES);
        drop(replacement);
        drop(second);
        for index in 0..MAX_DOCUMENTS * 4 {
            drop(
                cache
                    .insert(format!("small-{index}"), document(8))
                    .expect("bounded cache"),
            );
            assert!(cache.entries.len() <= MAX_DOCUMENTS);
            assert!(cache.bytes <= MAX_DOCUMENT_BYTES);
        }
    }
}
