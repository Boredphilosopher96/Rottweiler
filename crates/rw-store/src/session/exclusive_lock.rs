//! Exclusive ownership is released explicitly before the owner's descriptor closes.

use std::{fs::File, io};

pub(crate) struct ExclusiveFileLock(File);

impl ExclusiveFileLock {
    pub(crate) fn try_acquire(file: File) -> io::Result<Self> {
        loop {
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Self(file)),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn descriptor(&self) -> &File {
        &self.0
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        // CLOEXEC does not cover a concurrently forked child's pre-exec interval.
        // Closing only this FD could leave its shared description holding the lock.
        while let Err(rustix::io::Errno::INTR) =
            rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock)
        {}
    }
}
