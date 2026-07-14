//! Private, bounded cache for the sanitized provider-neutral model catalog.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rw_types::ModelCatalogSnapshot;
use thiserror::Error;

const MAX_CATALOG_CACHE_BYTES: usize = 2 * 1024 * 1024;
static CACHE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum ModelCatalogCacheError {
    #[error("model catalog cache path has no parent directory")]
    MissingParent,
    #[error("model catalog cache path is unsafe")]
    UnsafePath,
    #[error("model catalog cache exceeds its private size limit")]
    TooLarge,
    #[error("model catalog cache could not be read or written: {0}")]
    Io(#[from] std::io::Error),
    #[error("model catalog cache is invalid: {0}")]
    Invalid(#[from] serde_json::Error),
}

/// Loads a sanitized cached catalog without contacting any provider.
///
/// Missing cache files are normal. Unsafe, oversized, or malformed files are
/// reported so the caller can discard the cache and fall back to live discovery.
///
/// # Errors
///
/// Returns an error when the path is unsafe, the cache exceeds its bound, the
/// file cannot be read, or its JSON does not describe a catalog snapshot.
pub fn load_model_catalog_cache(
    path: &Path,
) -> Result<Option<ModelCatalogSnapshot>, ModelCatalogCacheError> {
    let Some(metadata) = validated_cache_metadata(path)? else {
        return Ok(None);
    };
    if metadata.len() > u64::try_from(MAX_CATALOG_CACHE_BYTES).unwrap_or(u64::MAX) {
        return Err(ModelCatalogCacheError::TooLarge);
    }
    let bytes = fs::read(path)?;
    if bytes.len() > MAX_CATALOG_CACHE_BYTES {
        return Err(ModelCatalogCacheError::TooLarge);
    }
    serde_json::from_slice(&bytes).map(Some).map_err(Into::into)
}

/// Atomically replaces the private sanitized catalog cache.
///
/// # Errors
///
/// Returns an error when the parent or target path is unsafe, serialization or
/// I/O fails, or the serialized catalog exceeds the private cache bound.
pub fn store_model_catalog_cache(
    path: &Path,
    snapshot: &ModelCatalogSnapshot,
) -> Result<(), ModelCatalogCacheError> {
    let parent = path.parent().ok_or(ModelCatalogCacheError::MissingParent)?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
        return Err(ModelCatalogCacheError::UnsafePath);
    }
    let bytes = serde_json::to_vec(snapshot)?;
    if bytes.len() > MAX_CATALOG_CACHE_BYTES {
        return Err(ModelCatalogCacheError::TooLarge);
    }
    // Validate an existing target before rename so a cache path cannot be
    // used to replace an attacker-controlled link or multi-link file.
    let _ = validated_cache_metadata(path)?;
    let temporary = allocate_temporary(parent)?;
    let result = (|| -> Result<(), std::io::Error> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        fs::File::open(parent)?.sync_all()
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn validated_cache_metadata(path: &Path) -> Result<Option<fs::Metadata>, ModelCatalogCacheError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ModelCatalogCacheError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
            return Err(ModelCatalogCacheError::UnsafePath);
        }
    }
    Ok(Some(metadata))
}

fn allocate_temporary(parent: &Path) -> Result<PathBuf, ModelCatalogCacheError> {
    for _ in 0..16 {
        let nonce = CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".model-catalog-{}-{nonce}.tmp", std::process::id()));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(ModelCatalogCacheError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate model catalog cache temporary",
    )))
}

#[cfg(test)]
mod tests {
    use rw_types::ModelCatalogSnapshot;
    use tempfile::tempdir;

    use super::*;

    fn snapshot() -> ModelCatalogSnapshot {
        ModelCatalogSnapshot {
            aliases: Vec::new(),
            models: Vec::new(),
            providers: Vec::new(),
            cached: false,
            truncated: false,
        }
    }

    #[test]
    fn private_catalog_cache_round_trips_and_remains_non_authoritative()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let path = root.path().join("model-catalog.json");
        assert_eq!(load_model_catalog_cache(&path)?, None);
        store_model_catalog_cache(&path, &snapshot())?;
        assert_eq!(load_model_catalog_cache(&path)?, Some(snapshot()));

        std::fs::write(&path, b"not-json")?;
        assert!(load_model_catalog_cache(&path).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn catalog_cache_rejects_symlink_and_hardlink_targets() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::os::unix::fs::symlink;

        let root = tempdir()?;
        let path = root.path().join("model-catalog.json");
        let outside = root.path().join("outside.json");
        std::fs::write(&outside, b"{}")?;
        symlink(&outside, &path)?;
        assert!(store_model_catalog_cache(&path, &snapshot()).is_err());
        std::fs::remove_file(&path)?;
        std::fs::hard_link(&outside, &path)?;
        assert!(store_model_catalog_cache(&path, &snapshot()).is_err());
        Ok(())
    }
}
