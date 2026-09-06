use super::{
    BlobWriteGuard, CheckpointError, CheckpointOperation, MAX_BLOBS, MAX_CAPTURE_FILE_BYTES,
};
use crate::checkpoint::{CAPTURE_CHUNK_BYTES, CheckpointFileState};
use std::{
    fs::{self, File},
    io::{Read, Write},
};

impl BlobWriteGuard<'_> {
    pub(in crate::checkpoint) fn capture(
        &mut self,
        reader: &mut impl Read,
        unix_mode: Option<u32>,
        operation: &mut CheckpointOperation,
    ) -> Result<CheckpointFileState, CheckpointError> {
        // The single writer reserves staging separately from retained content.
        // This permits deduplication even when retained storage is full.
        self.connection.execute(
            "UPDATE quota SET staged=?1 WHERE id=1",
            [MAX_CAPTURE_FILE_BYTES],
        )?;
        let mut temporary = tempfile::Builder::new()
            .prefix("capture-")
            .tempfile_in(self.owner.root.join("staging"))?;
        let mut hash = blake3::Hasher::new();
        let mut bytes = 0_u64;
        let mut chunk = vec![0_u8; CAPTURE_CHUNK_BYTES].into_boxed_slice();
        loop {
            operation.check()?;
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            if bytes > MAX_CAPTURE_FILE_BYTES {
                return Err(CheckpointError::CaptureFileLimit);
            }
            operation.capture(count)?;
            hash.update(&chunk[..count]);
            temporary.write_all(&chunk[..count])?;
        }
        let digest = hash.finalize().to_hex().to_string();
        let directory = self.owner.directory().join(&digest[..2]);
        let path = directory.join(&digest);
        if path.exists() {
            let existing = File::open(&path)?;
            if existing.metadata()?.len() != bytes
                || operation.hash(existing.take(bytes + 1))?.to_hex().as_str() != digest
            {
                return Err(CheckpointError::CorruptBlob);
            }
            drop(temporary);
        } else {
            self.admit(bytes, operation)?;
            fs::create_dir_all(&directory)?;
            File::open(self.owner.directory())?.sync_all()?;
            temporary.as_file().sync_all()?;
            temporary
                .persist_noclobber(&path)
                .map_err(|error| error.error)?;
            File::open(&directory)?.sync_all()?;
        }
        File::open(self.owner.root.join("staging"))?.sync_all()?;
        self.connection.execute(
            "INSERT OR IGNORE INTO blobs VALUES(?1,?2)",
            rusqlite::params![digest, bytes],
        )?;
        self.connection
            .execute("INSERT OR IGNORE INTO protected VALUES(?1)", [&digest])?;
        self.connection
            .execute("UPDATE quota SET staged=0 WHERE id=1", [])?;
        Ok(CheckpointFileState::Present {
            blob: digest,
            bytes,
            unix_mode,
        })
    }

    fn admit(
        &mut self,
        bytes: u64,
        operation: &mut CheckpointOperation,
    ) -> Result<(), CheckpointError> {
        if !self.fits(bytes)? {
            self.reconcile(operation, false)?;
        }
        if !self.fits(bytes)? {
            return Err(CheckpointError::BlobQuotaExceeded);
        }
        Ok(())
    }

    fn fits(&self, bytes: u64) -> Result<bool, CheckpointError> {
        let (used, count): (u64, u64) = self.connection.query_row(
            "SELECT used_bytes,blob_count FROM quota WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(bytes <= self.owner.retained_bytes.saturating_sub(used) && count < MAX_BLOBS)
    }
}
