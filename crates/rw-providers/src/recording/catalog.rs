//! One-file catalog verification; directory size does not become retained memory.
use super::{
    CapabilityManifest, RecordFixture, RecordedCapabilities, capability_manifest_path,
    fixture_matches_manifest, provider_hash, replay_reads, validate_manifest,
};
use crate::{ProviderError, ProviderErrorKind, ProviderModelMetadata};
use rw_types::json_structure::{JsonStructureLimits, preflight_json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const CATALOG_DEADLINE: Duration = Duration::from_secs(30);
pub(super) const MANIFEST_BYTES: usize = crate::types::MAX_PROVIDER_MODEL_CATALOG_BYTES;
// Includes serde tagged-content intermediates and vector/string growth. Source
// bytes are separately limited to 64 MiB and released before the next file.
const DECODE_BYTES: usize = 4 * replay_reads::MAX_FIXTURE_BYTES;
struct CatalogPool {
    verifier: Arc<tokio::sync::Semaphore>,
    waiters: Arc<tokio::sync::Semaphore>,
}
static CATALOG: OnceLock<CatalogPool> = OnceLock::new();

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(super) async fn load(
    directory: PathBuf,
    provider: String,
) -> Result<(RecordedCapabilities, Option<ProviderModelMetadata>), ProviderError> {
    let pool = CATALOG.get_or_init(|| CatalogPool {
        verifier: Arc::new(tokio::sync::Semaphore::new(1)),
        waiters: Arc::new(tokio::sync::Semaphore::new(64)),
    });
    let waiting = Arc::clone(&pool.waiters)
        .try_acquire_owned()
        .map_err(|_| exhausted("recording catalog verification queue is full"))?;
    let deadline = Instant::now() + CATALOG_DEADLINE;
    let admitted =
        tokio::time::timeout_at(deadline.into(), Arc::clone(&pool.verifier).acquire_owned())
            .await
            .map_err(|_| exhausted("recording catalog admission exceeded its deadline"))?
            .map_err(|_| exhausted("recording catalog verifier is closed"))?;
    drop(waiting);
    let cancelled = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let flag = Arc::clone(&cancelled.0);
    let work = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        let _admitted = admitted;
        scan(&directory, &provider, || {
            if flag.load(Ordering::Acquire) {
                Err(ProviderError::new(
                    ProviderErrorKind::Cancelled,
                    "recording catalog verification cancelled",
                ))
            } else if Instant::now() >= deadline {
                Err(exhausted(
                    "recording catalog verification exceeded its deadline",
                ))
            } else {
                Ok(())
            }
        })
    });
    tokio::time::timeout_at(deadline.into(), work)
        .await
        .map_err(|_| exhausted("recording catalog verification exceeded its deadline"))?
        .map_err(|error| exhausted(&error.to_string()))?
}

fn scan(
    directory: &Path,
    provider: &str,
    check: impl Fn() -> Result<(), ProviderError>,
) -> Result<(RecordedCapabilities, Option<ProviderModelMetadata>), ProviderError> {
    check()?;
    let path = capability_manifest_path(directory, provider);
    let bytes = replay_reads::read_bounded(&path, MANIFEST_BYTES, &check)?;
    let manifest = decode_manifest(&bytes)?;
    drop(bytes);
    validate_manifest(provider, &manifest)?;
    let prefix = format!("{}-", provider_hash(provider));
    let manifest_name = format!("{}-capabilities.json", provider_hash(provider));
    let mut found = false;
    for entry in std::fs::read_dir(directory).map_err(super::record_io_error)? {
        check()?;
        let entry = entry.map_err(super::record_io_error)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".json") || name == manifest_name {
            continue;
        }
        let bytes =
            replay_reads::read_bounded(&entry.path(), replay_reads::MAX_FIXTURE_BYTES, &check)?;
        check()?;
        let fixture = decode_fixture(&bytes)?;
        fixture.validate()?;
        if fixture.provider != provider
            || entry.path()
                != super::fixture_path(
                    directory,
                    provider,
                    &fixture.request_hash,
                    fixture.occurrence,
                )
            || !fixture_matches_manifest(
                &fixture,
                &manifest.capabilities,
                manifest.model_metadata.as_ref(),
            )
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "replay fixture differs from its required provider capability manifest",
            ));
        }
        found = true;
    }
    check()?;
    if !found {
        return Err(ProviderError::new(
            ProviderErrorKind::ReplayMiss,
            "no completed replay fixtures exist for provider",
        ));
    }
    Ok((manifest.capabilities, manifest.model_metadata))
}

pub(super) fn decode_fixture(bytes: &[u8]) -> Result<RecordFixture, ProviderError> {
    admit(bytes, replay_reads::MAX_FIXTURE_BYTES)?;
    serde_json::from_slice(bytes).map_err(|error| invalid(&error))
}
pub(super) fn decode_manifest(bytes: &[u8]) -> Result<CapabilityManifest, ProviderError> {
    admit(bytes, MANIFEST_BYTES)?;
    serde_json::from_slice(bytes).map_err(|error| invalid(&error))
}
pub(super) fn admit_fixture(bytes: &[u8]) -> Result<(), ProviderError> {
    admit(bytes, replay_reads::MAX_FIXTURE_BYTES)
}
pub(super) fn admit_manifest(bytes: &[u8]) -> Result<(), ProviderError> {
    admit(bytes, MANIFEST_BYTES)
}

pub(super) fn encode_manifest(value: &CapabilityManifest) -> Result<Vec<u8>, ProviderError> {
    let bytes = encode(value, MANIFEST_BYTES)?;
    admit_manifest(&bytes)?;
    Ok(bytes)
}
pub(super) fn encode_fixture(value: &RecordFixture) -> Result<Vec<u8>, ProviderError> {
    let bytes = encode(value, replay_reads::MAX_FIXTURE_BYTES)?;
    admit_fixture(&bytes)?;
    Ok(bytes)
}
fn encode(value: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = Vec::new();
    let mut writer = rw_types::json_encoding::JsonWriter::buffer(&mut bytes, limit, 1024)
        .map_err(|_| exhausted("recording encoded allocation admission exceeded"))?;
    serde_json::to_writer_pretty(&mut writer, value).map_err(|error| invalid(&error))?;
    Ok(bytes)
}

fn admit(bytes: &[u8], encoded_limit: usize) -> Result<(), ProviderError> {
    let shape = preflight_json(
        bytes,
        JsonStructureLimits {
            max_encoded_bytes: encoded_limit,
            max_nodes: encoded_limit / 128,
            max_string_bytes: encoded_limit,
            max_depth: 128,
        },
    )
    .map_err(|error| invalid(&error))?;
    // Direct JSON owns all map storage. Typed slot accounting additionally covers
    // the largest source container and serde's tagged-content intermediates; JSON
    // string bytes and vector growth are included in the shared structural owner.
    let slot = [
        size_of::<RecordFixture>(),
        size_of::<crate::ProviderRequest>(),
        size_of::<rw_types::Turn>(),
        size_of::<rw_types::Block>(),
        size_of::<rw_types::TurnMeta>(),
        size_of::<crate::ToolDefinition>(),
        size_of::<crate::ProviderEvent>(),
        size_of::<super::RecordedItem>(),
        size_of::<super::RawSseFrame>(),
        size_of::<crate::ModelPricing>(),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    let decoded = shape.direct_value_decode_bytes().and_then(|json| {
        shape
            .nodes
            .checked_mul(slot.checked_mul(4)?)
            .and_then(|slots| slots.checked_add(json))
    });
    if decoded.is_none_or(|bytes| bytes > DECODE_BYTES) {
        return Err(exhausted("recording exceeds decoded allocation admission"));
    }
    Ok(())
}
fn invalid(error: &serde_json::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        format!("invalid recording: {error}"),
    )
}
fn exhausted(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::ResourceExhausted, message)
}

#[cfg(test)]
mod tests;
