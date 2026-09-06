//! Finite stable-index workspace generations, admitted before typed decoding.
use super::{CHECKPOINT_ROOTS_VERSION, CheckpointRootMapping, MAX_WORKSPACE_ROOTS};
use miette::{IntoDiagnostic, Result, miette};
use rw_types::json_structure::{JsonStructureLimits, preflight_json};
use std::{
    io::{self, Read},
    path::Path,
};

const MAX_MAPPING_BYTES: usize = 16 * 1024 * 1024;
const MAX_MAPPING_NODES: usize = 4 * MAX_WORKSPACE_ROOTS * MAX_WORKSPACE_ROOTS;

pub(super) fn read(path: &Path) -> io::Result<Vec<u8>> {
    read_limit(path, MAX_MAPPING_BYTES)
}
pub(super) fn read_limit(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    let file = std::fs::File::from(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?);
    #[cfg(not(unix))]
    let file = {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::Error::other("checkpoint mapping is a symlink"));
        }
        std::fs::File::open(path)?
    };
    let metadata = file.metadata()?;
    let length = usize::try_from(metadata.len()).map_err(io::Error::other)?;
    if !metadata.is_file() || length > limit {
        return Err(io::Error::other("checkpoint mapping encoded admission"));
    }
    let mut bytes = Vec::with_capacity(length + 1);
    file.take((length + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() != length {
        return Err(io::Error::other("checkpoint mapping changed during read"));
    }
    Ok(bytes)
}

pub(super) fn decode(bytes: &[u8]) -> Result<CheckpointRootMapping> {
    preflight_json(
        bytes,
        JsonStructureLimits {
            max_encoded_bytes: MAX_MAPPING_BYTES,
            max_nodes: MAX_MAPPING_NODES,
            max_string_bytes: MAX_MAPPING_BYTES,
            max_depth: 5,
        },
    )
    .into_diagnostic()?;
    // The closed graph contains only the mapping, at most32 generation structs,
    // their PathBuf vectors and path strings. No generic Value/untagged trial
    // decoder owns another payload graph. The borrowed visitor bounds strings
    // and vector elements before serde can construct them.
    let mapping: CheckpointRootMapping = serde_json::from_slice(bytes).into_diagnostic()?;
    validate(&mapping)?;
    Ok(mapping)
}

pub(super) fn validate(mapping: &CheckpointRootMapping) -> Result<()> {
    if mapping.version != CHECKPOINT_ROOTS_VERSION
        || mapping.generations.is_empty()
        || mapping.generations.len() > MAX_WORKSPACE_ROOTS
    {
        return Err(miette!("checkpoint mapping schema or generation count"));
    }
    for (index, entry) in mapping.generations.iter().enumerate() {
        if entry.roots.is_empty()
            || entry.roots.len() > MAX_WORKSPACE_ROOTS
            || entry.roots.iter().any(|root| !root.is_absolute())
            || (!entry.committed && index + 1 != mapping.generations.len())
        {
            return Err(miette!("checkpoint generation roots or commit state"));
        }
        if let Some(previous) = index
            .checked_sub(1)
            .map(|prior| &mapping.generations[prior])
            && (previous.generation.checked_add(1) != Some(entry.generation)
                || entry.effective_from_turn < previous.effective_from_turn
                || entry.roots.len() != previous.roots.len() + 1
                || !entry.roots.starts_with(&previous.roots))
        {
            return Err(miette!(
                "checkpoint generations require stable-index append"
            ));
        }
    }
    Ok(())
}

pub(super) fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    struct Writer(Vec<u8>);
    impl io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let next = self
                .0
                .len()
                .checked_add(bytes.len())
                .filter(|next| *next <= MAX_MAPPING_BYTES)
                .ok_or_else(|| io::Error::other("private JSON encoded admission"))?;
            if next > self.0.capacity() {
                let capacity = next.next_power_of_two().clamp(4096, MAX_MAPPING_BYTES);
                self.0
                    .try_reserve_exact(capacity - self.0.len())
                    .map_err(io::Error::other)?;
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer(Vec::new());
    serde_json::to_writer_pretty(&mut writer, value).into_diagnostic()?;
    Ok(writer.0)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::session_runtime::checkpoint_journal::CheckpointRootGeneration;
    fn valid() -> CheckpointRootMapping {
        CheckpointRootMapping {
            version: CHECKPOINT_ROOTS_VERSION,
            generations: vec![CheckpointRootGeneration {
                generation: 4,
                effective_from_turn: 9,
                roots: vec![std::env::temp_dir()],
                committed: true,
            }],
        }
    }
    #[test]
    fn mapping_checks_source_size_before_decode_and_validates_generation_topology() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("mapping.json");
        let file = std::fs::File::create(&path).expect("file");
        file.set_len(MAX_MAPPING_BYTES as u64 + 1)
            .expect("oversized descriptor");
        assert!(read(&path).is_err());
        let mapping = valid();
        let encoded = encode(&mapping).expect("encode");
        std::fs::write(&path, &encoded).expect("write");
        assert_eq!(
            decode(&read(&path).expect("bounded read")).expect("decode"),
            mapping
        );
        let mut invalid = valid();
        invalid.generations[0].roots.clear();
        assert!(decode(&encode(&invalid).expect("fixture")).is_err());
        invalid = valid();
        invalid.generations.push(invalid.generations[0].clone());
        assert!(decode(&encode(&invalid).expect("fixture")).is_err());
        let nested = format!("{}0{}", "[".repeat(64), "]".repeat(64));
        assert!(decode(nested.as_bytes()).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn mapping_read_rejects_symlink_before_opening_target() {
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("target");
        let link = root.path().join("link");
        std::fs::write(&target, encode(&valid()).expect("encode")).expect("target");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        assert!(read(&link).is_err());
    }
}
