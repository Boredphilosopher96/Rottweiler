//! A pinned regular file with an admitted immutable-length read window.
use miette::{Result, miette};
use rw_providers::MAX_RECORDING_FIXTURE_BYTES;
use std::{
    fs::{File, Metadata},
    io::Read,
};

pub(in super::super) struct FixtureRead {
    file: File,
    metadata: Metadata,
    bytes: usize,
}
impl FixtureRead {
    pub(super) fn new(file: File, metadata: Metadata) -> Result<Self> {
        let bytes = usize::try_from(metadata.len())
            .map_err(|_| miette!("web-search fixture length is not representable"))?;
        if bytes > MAX_RECORDING_FIXTURE_BYTES {
            return Err(miette!(
                "web-search fixture exceeds the {MAX_RECORDING_FIXTURE_BYTES}-byte recording limit"
            ));
        }
        Ok(Self {
            file,
            metadata,
            bytes,
        })
    }
    pub(in super::super) fn read(mut self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(self.bytes)
            .map_err(|_| miette!("web-search fixture read allocation unavailable"))?;
        bytes.resize(self.bytes, 0);
        self.file
            .read_exact(&mut bytes)
            .map_err(|error| miette!("web-search fixture changed or could not read: {error}"))?;
        let mut extra = [0_u8; 1];
        let trailing = self
            .file
            .read(&mut extra)
            .map_err(|error| miette!("web-search fixture growth check failed: {error}"))?;
        let after = self
            .file
            .metadata()
            .map_err(|error| miette!("web-search fixture final validation failed: {error}"))?;
        if trailing != 0
            || after.len() != self.metadata.len()
            || after.modified().ok() != self.metadata.modified().ok()
        {
            return Err(miette!("web-search fixture changed during its pinned read"));
        }
        Ok(bytes)
    }
}
