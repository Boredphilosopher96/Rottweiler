//! Advisory ownership is released explicitly before the owner's descriptor closes.

use std::{fs::File, io};

#[derive(Debug)]
pub(crate) struct AdvisoryFileLock(File);

impl AdvisoryFileLock {
    pub(crate) fn try_exclusive(file: File) -> io::Result<Self> {
        Self::try_acquire(file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
    }

    pub(crate) fn try_shared(file: File) -> io::Result<Self> {
        Self::try_acquire(file, rustix::fs::FlockOperation::NonBlockingLockShared)
    }

    fn try_acquire(file: File, operation: rustix::fs::FlockOperation) -> io::Result<Self> {
        loop {
            match rustix::fs::flock(&file, operation) {
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

impl Drop for AdvisoryFileLock {
    fn drop(&mut self) {
        // CLOEXEC does not cover a concurrently forked child's pre-exec interval.
        // Closing only this FD could leave its shared description holding the lock.
        while let Err(rustix::io::Errno::INTR) =
            rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock)
        {}
    }
}
