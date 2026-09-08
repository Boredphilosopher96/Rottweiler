//! Length- and chunk-authenticated payload format; no full-body read is needed for a window.
use super::check_cancelled;
use rw_types::{SessionPayloadReference, session_payload::MAX_SESSION_PAYLOAD_BYTES};
use std::{
    fs::{File, Metadata},
    io,
    os::unix::fs::{FileExt as _, MetadataExt as _},
};

pub(super) const CHUNK_BYTES: usize = 64 * 1024;
const MAGIC: &[u8; 8] = b"RWPLD001";
const HEADER_BYTES: usize = 16;
const MAX_CHUNKS: usize = MAX_SESSION_PAYLOAD_BYTES.div_ceil(CHUNK_BYTES);

#[derive(Clone, Debug)]
pub(super) struct Manifest {
    pub(super) bytes: usize,
    pub(super) chunks: Vec<[u8; 32]>,
}
impl Manifest {
    pub(super) fn from_bytes(bytes: &[u8], cancelled: &dyn Fn() -> bool) -> io::Result<Self> {
        if bytes.len() > MAX_SESSION_PAYLOAD_BYTES {
            return Err(corrupt("payload exceeds byte limit"));
        }
        std::str::from_utf8(bytes).map_err(|_| corrupt("payload is not UTF-8"))?;
        let mut chunks = Vec::with_capacity(bytes.len().div_ceil(CHUNK_BYTES));
        for chunk in bytes.chunks(CHUNK_BYTES) {
            check_cancelled(cancelled)?;
            chunks.push(*blake3::hash(chunk).as_bytes());
        }
        Ok(Self {
            bytes: bytes.len(),
            chunks,
        })
    }
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(self.header_bytes());
        encoded.extend_from_slice(MAGIC);
        encoded.extend_from_slice(&(self.bytes as u64).to_le_bytes());
        for hash in &self.chunks {
            encoded.extend_from_slice(hash);
        }
        encoded
    }
    pub(super) fn reference(&self) -> SessionPayloadReference {
        SessionPayloadReference {
            digest: blake3::hash(&self.encode()).to_hex().to_string(),
            bytes: self.bytes,
        }
    }
    pub(super) fn read(file: &File) -> io::Result<Self> {
        checked_file(file, 0o400)?;
        let mut header = [0_u8; HEADER_BYTES];
        file.read_exact_at(&mut header, 0)?;
        if &header[..8] != MAGIC {
            return Err(corrupt("invalid payload format"));
        }
        let bytes = u64::from_le_bytes(
            header[8..16]
                .try_into()
                .map_err(|_| corrupt("invalid payload length"))?,
        );
        let bytes = usize::try_from(bytes).map_err(|_| corrupt("payload length overflow"))?;
        let count = bytes.div_ceil(CHUNK_BYTES);
        if bytes > MAX_SESSION_PAYLOAD_BYTES || count > MAX_CHUNKS {
            return Err(corrupt("invalid payload manifest bounds"));
        }
        let mut encoded = vec![0_u8; count * 32];
        file.read_exact_at(&mut encoded, HEADER_BYTES as u64)?;
        let mut chunks = Vec::with_capacity(count);
        for encoded_hash in encoded.chunks_exact(32) {
            let mut hash = [0_u8; 32];
            hash.copy_from_slice(encoded_hash);
            chunks.push(hash);
        }
        Ok(Self { bytes, chunks })
    }
    fn header_bytes(&self) -> usize {
        HEADER_BYTES + self.chunks.len() * 32
    }
}

pub(super) struct PayloadReader {
    file: File,
    snapshot: Metadata,
    pub(super) manifest: Manifest,
    caches: [(Option<usize>, Vec<u8>); 2],
    next_cache: usize,
}
impl PayloadReader {
    pub(super) fn open(file: File, reference: &SessionPayloadReference) -> io::Result<Self> {
        let snapshot = checked_file(&file, 0o400)?;
        let manifest = Manifest::read(&file)?;
        if manifest.reference() != *reference {
            return Err(corrupt("payload identity mismatch"));
        }
        check_length(&file, &manifest)?;
        let reader = Self {
            file,
            snapshot,
            manifest,
            caches: [(None, vec![0; CHUNK_BYTES]), (None, vec![0; CHUNK_BYTES])],
            next_cache: 0,
        };
        reader.verify_snapshot()?;
        Ok(reader)
    }
    pub(super) fn chunk(&mut self, index: usize) -> io::Result<&[u8]> {
        if index >= self.manifest.chunks.len() {
            return Err(corrupt("payload chunk outside source"));
        }
        let length = CHUNK_BYTES.min(self.manifest.bytes - index * CHUNK_BYTES);
        if let Some(slot) = self
            .caches
            .iter()
            .position(|(cached, _)| *cached == Some(index))
        {
            return Ok(&self.caches[slot].1[..length]);
        }
        self.verify_snapshot()?;
        let slot = self.next_cache;
        self.next_cache = 1 - slot;
        let bytes = &mut self.caches[slot].1[..length];
        self.file.read_exact_at(
            bytes,
            (self.manifest.header_bytes() + index * CHUNK_BYTES) as u64,
        )?;
        if blake3::hash(bytes).as_bytes() != &self.manifest.chunks[index] {
            return Err(corrupt("payload chunk checksum mismatch"));
        }
        self.verify_snapshot()?;
        self.caches[slot].0 = Some(index);
        Ok(&self.caches[slot].1[..length])
    }
    pub(super) fn verify_all(&mut self, cancelled: &dyn Fn() -> bool) -> io::Result<()> {
        for index in 0..self.manifest.chunks.len() {
            check_cancelled(cancelled)?;
            self.chunk(index)?;
        }
        self.verify_snapshot()
    }
    pub(super) fn verify_snapshot(&self) -> io::Result<()> {
        let after = checked_file(&self.file, 0o400)?;
        let before = &self.snapshot;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        ) {
            return Err(corrupt("payload changed during read"));
        }
        Ok(())
    }
}

pub(super) fn checked_file(file: &File, mode: u32) -> io::Result<Metadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != mode
    {
        return Err(corrupt("unsafe session payload file"));
    }
    Ok(metadata)
}
pub(super) fn check_length(file: &File, manifest: &Manifest) -> io::Result<()> {
    if file.metadata()?.len() != (manifest.header_bytes() + manifest.bytes) as u64 {
        return Err(corrupt("payload file length mismatch"));
    }
    Ok(())
}
pub(super) fn open_file(
    parent: &File,
    name: &str,
    flags: rustix::fs::OFlags,
    mode: rustix::fs::Mode,
) -> io::Result<File> {
    Ok(File::from(rustix::fs::openat(
        parent,
        name,
        flags
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        mode,
    )?))
}
pub(super) fn corrupt(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
