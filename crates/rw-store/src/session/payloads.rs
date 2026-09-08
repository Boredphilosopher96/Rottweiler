//! Session-owned immutable payloads. Journal references retain these until session deletion.
mod format;
#[cfg(test)]
mod tests;
mod window;

use super::AdvisoryFileLock;
use format::{Manifest, PayloadReader, checked_file, corrupt, open_file};
use rw_types::SessionPayloadReference;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Write as _},
    sync::{Arc, Mutex},
};

pub use window::PayloadWindow;
/// Maximum durable payload objects in one session, including unpublished journal attachments.
pub const MAX_SESSION_PAYLOADS: usize = 512;
/// Sum of logical immutable payload bytes retained by one session.
pub const MAX_SESSION_PAYLOAD_TOTAL_BYTES: usize = 256 * 1024 * 1024;
/// Admission covers manifests, two verified chunk buffers, bounded output and query state.
pub const PAYLOAD_WINDOW_WORKING_BYTES: usize = 512 * 1024;
const DIRECTORY: &str = "payloads";
const STAGING: &str = "pending";

/// Descriptor-bound storage. Every physical job retains this owner and its exclusive lock.
#[derive(Clone, Debug)]
pub struct SessionPayloadStore(Arc<Inner>);
#[derive(Debug)]
struct Inner {
    parent: File,
    root: File,
    identity: (rustix::fs::Dev, u64),
    records: Mutex<BTreeMap<String, usize>>,
    _lock: AdvisoryFileLock,
}

impl SessionPayloadStore {
    pub(super) fn open(parent: File) -> io::Result<Self> {
        let root = super::open_or_create_directory(&parent, DIRECTORY).map_err(io::Error::other)?;
        let stat = rustix::fs::fstat(&root)?;
        if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o777 != 0o700 {
            return Err(corrupt("unsafe session payload directory"));
        }
        let lock_file = open_file(
            &root,
            "lock",
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE,
            0o600,
        )?;
        checked_file(&lock_file, 0o600)?;
        let lock = AdvisoryFileLock::try_exclusive(lock_file)?;
        // A crash may leave only the private staging object. Published objects are never purged.
        match open_file(&root, STAGING, rustix::fs::OFlags::RDONLY, 0) {
            Ok(file) => {
                checked_file(&file, 0o600).or_else(|_| checked_file(&file, 0o400))?;
                rustix::fs::unlinkat(&root, STAGING, rustix::fs::AtFlags::empty())?;
                root.sync_all()?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let records = inventory(&root)?;
        Ok(Self(Arc::new(Inner {
            parent,
            root,
            identity: (stat.st_dev, stat.st_ino),
            records: Mutex::new(records),
            _lock: lock,
        })))
    }

    /// Writes an immutable payload before its reference can enter the session journal.
    /// The caller owns input allocation and runs this finite operation in an admitted worker.
    /// # Errors
    /// Rejects cancellation, quota, namespace replacement, unsafe files and I/O failures.
    pub fn write(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<SessionPayloadReference> {
        self.validate_namespace()?;
        let manifest = Manifest::from_bytes(bytes, cancelled)?;
        let reference = manifest.reference();
        let mut records = self
            .0
            .records
            .lock()
            .map_err(|_| corrupt("payload quota poisoned"))?;
        if records.contains_key(&reference.digest) {
            self.reader(&reference)?.verify_all(cancelled)?;
            self.0.root.sync_all()?;
            return Ok(reference);
        }
        self.publish_reserved(&manifest, &mut records, cancelled, |file| {
            for chunk in bytes.chunks(format::CHUNK_BYTES) {
                check_cancelled(cancelled)?;
                file.write_all(chunk)?;
            }
            Ok(())
        })?;
        Ok(reference)
    }

    /// Copies a referenced payload into another session without shared mutable file identities.
    /// Only references present in the selected canonical fork prefix should call this method.
    /// # Errors
    /// Rejects cancellation, corruption, target quota and publication failures.
    pub fn copy_to(
        &self,
        reference: &SessionPayloadReference,
        target: &Self,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        if Arc::ptr_eq(&self.0, &target.0) {
            self.reader(reference)?.verify_all(cancelled)?;
            return self.0.root.sync_all();
        }
        let mut reader = self.reader(reference)?;
        target.validate_namespace()?;
        let mut records = target
            .0
            .records
            .lock()
            .map_err(|_| corrupt("payload quota poisoned"))?;
        if records.contains_key(&reference.digest) {
            target.reader(reference)?.verify_all(cancelled)?;
            target.0.root.sync_all()?;
            return Ok(());
        }
        let manifest = reader.manifest.clone();
        target.publish_reserved(&manifest, &mut records, cancelled, |file| {
            for index in 0..manifest.chunks.len() {
                check_cancelled(cancelled)?;
                file.write_all(reader.chunk(index)?)?;
            }
            reader.verify_snapshot()
        })?;
        Ok(())
    }

    /// Reads at most the shared output ceiling; query matching uses bounded streaming state.
    /// # Errors
    /// Rejects malformed references, invalid cursors/queries, cancellation and changed bytes.
    pub fn window(
        &self,
        reference: &SessionPayloadReference,
        offset: usize,
        query: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<PayloadWindow> {
        let mut reader = self.reader(reference)?;
        let window = window::read(&mut reader, offset, query, cancelled)?;
        reader.verify_snapshot()?;
        Ok(window)
    }

    fn reader(&self, reference: &SessionPayloadReference) -> io::Result<PayloadReader> {
        self.validate_namespace()?;
        reference.validate().map_err(corrupt)?;
        let file = open_file(
            &self.0.root,
            &format!("{}.payload", reference.digest),
            rustix::fs::OFlags::RDONLY,
            0,
        )?;
        PayloadReader::open(file, reference)
    }

    fn validate_namespace(&self) -> io::Result<()> {
        let current = open_file(
            &self.0.parent,
            DIRECTORY,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            0,
        )?;
        let stat = rustix::fs::fstat(&current)?;
        if (stat.st_dev, stat.st_ino) != self.0.identity {
            return Err(corrupt("session payload namespace replaced"));
        }
        Ok(())
    }

    fn publish_reserved(
        &self,
        manifest: &Manifest,
        records: &mut BTreeMap<String, usize>,
        cancelled: &dyn Fn() -> bool,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        let reference = manifest.reference();
        admit(records, reference.bytes)?;
        records.insert(reference.digest.clone(), reference.bytes);
        let mut publication_attempted = false;
        let result = self.publish(manifest, cancelled, &mut publication_attempted, write);
        if result.is_err() && !publication_attempted && self.prove_unpublished(&reference) {
            records.remove(&reference.digest);
        }
        result
    }

    fn prove_unpublished(&self, reference: &SessionPayloadReference) -> bool {
        let absent = |name: &str| {
            matches!(
                rustix::fs::statat(&self.0.root, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW),
                Err(rustix::io::Errno::NOENT)
            )
        };
        self.validate_namespace().is_ok()
            && absent(STAGING)
            && absent(&format!("{}.payload", reference.digest))
            && self.0.root.sync_all().is_ok()
    }

    fn publish(
        &self,
        manifest: &Manifest,
        cancelled: &dyn Fn() -> bool,
        publication_attempted: &mut bool,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        check_cancelled(cancelled)?;
        let mut file = open_file(
            &self.0.root,
            STAGING,
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL,
            0o600,
        )?;
        let result = (|| {
            file.write_all(&manifest.encode())?;
            write(&mut file)?;
            file.sync_all()?;
            rustix::fs::fchmod(&file, rustix::fs::Mode::from_raw_mode(0o400))?;
            file.sync_all()?;
            check_cancelled(cancelled)?;
            self.validate_namespace()?;
            let name = format!("{}.payload", manifest.reference().digest);
            *publication_attempted = true;
            rustix::fs::renameat_with(
                &self.0.root,
                STAGING,
                &self.0.root,
                name,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
            #[cfg(test)]
            tests::publication_sync_fault()?;
            self.0.root.sync_all()
        })();
        if result.is_err() {
            // Any post-rename fsync failure leaves an immutable, quota-counted object on reopen.
            match rustix::fs::unlinkat(&self.0.root, STAGING, rustix::fs::AtFlags::empty()) {
                Ok(()) => self.0.root.sync_all()?,
                Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(error.into()),
            }
        }
        result
    }
}

fn inventory(root: &File) -> io::Result<BTreeMap<String, usize>> {
    let mut records = BTreeMap::new();
    let mut entries = rustix::fs::Dir::read_from(root)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| corrupt("invalid payload name"))?;
        if matches!(name, "." | ".." | "lock") {
            continue;
        }
        let digest = name
            .strip_suffix(".payload")
            .ok_or_else(|| corrupt("unknown session payload entry"))?;
        let file = open_file(root, name, rustix::fs::OFlags::RDONLY, 0)?;
        let manifest = Manifest::read(&file)?;
        let reference = manifest.reference();
        if digest != reference.digest {
            return Err(corrupt("payload manifest identity mismatch"));
        }
        checked_file(&file, 0o400)?;
        format::check_length(&file, &manifest)?;
        admit(&records, reference.bytes)?;
        records.insert(reference.digest, reference.bytes);
    }
    Ok(records)
}

fn admit(records: &BTreeMap<String, usize>, bytes: usize) -> io::Result<()> {
    if records.len() >= MAX_SESSION_PAYLOADS
        || records.values().sum::<usize>().saturating_add(bytes) > MAX_SESSION_PAYLOAD_TOTAL_BYTES
    {
        return Err(io::Error::other("session payload quota exceeded"));
    }
    Ok(())
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "payload operation cancelled",
        ))
    } else {
        Ok(())
    }
}
