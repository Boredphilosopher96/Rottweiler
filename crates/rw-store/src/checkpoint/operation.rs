//! One operation's aggregate allowance, shared across every workspace root.
use super::{CAPTURE_CHUNK_BYTES, CheckpointError};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_PATHS: usize = 100_000;
const MAX_PATH_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_HASH_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CAPTURE_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const MAX_METADATA_BYTES: usize = 32 * 1024 * 1024;

/// Cancellation handle for a checkpoint worker. Cancelling never releases ownership.
#[derive(Clone, Debug)]
pub struct CheckpointCancellation(Arc<AtomicBool>);

impl CheckpointCancellation {
    /// Ask the owning worker to stop at its next bounded I/O boundary.
    pub(super) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Finite scan and capture allowance for one coordinated filesystem operation.
#[derive(Debug)]
pub struct CheckpointOperation {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    paths: usize,
    path_bytes: usize,
    hash_bytes: u64,
    capture_bytes: u64,
}

impl Default for CheckpointOperation {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + Duration::from_secs(30),
            paths: 0,
            path_bytes: 0,
            hash_bytes: 0,
            capture_bytes: 0,
        }
    }
}

impl CheckpointOperation {
    /// Obtain a handle without transferring the worker's aggregate allowance.
    #[must_use]
    pub fn cancellation(&self) -> CheckpointCancellation {
        CheckpointCancellation(Arc::clone(&self.cancelled))
    }

    pub(super) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(super) fn check(&self) -> Result<(), CheckpointError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(CheckpointError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(CheckpointError::OperationLimit("30 second deadline"));
        }
        Ok(())
    }

    pub(super) fn path(&mut self, path: &str) -> Result<(), CheckpointError> {
        self.check()?;
        if self.paths >= MAX_PATHS
            || path.len() > MAX_PATH_BYTES.saturating_sub(self.path_bytes)
            || path.split('/').count() > MAX_DEPTH
        {
            return Err(CheckpointError::OperationLimit(
                "path count, bytes or depth",
            ));
        }
        self.paths += 1;
        self.path_bytes += path.len();
        Ok(())
    }

    pub(super) fn capture(&mut self, bytes: usize) -> Result<(), CheckpointError> {
        self.check()?;
        charge(
            &mut self.capture_bytes,
            bytes as u64,
            MAX_CAPTURE_BYTES,
            "256 MiB captured bytes",
        )
    }

    pub(super) fn hash(&mut self, mut reader: impl Read) -> Result<blake3::Hash, CheckpointError> {
        let mut hash = blake3::Hasher::new();
        let mut chunk = vec![0; CAPTURE_CHUNK_BYTES];
        loop {
            self.check()?;
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                return Ok(hash.finalize());
            }
            charge(
                &mut self.hash_bytes,
                count as u64,
                MAX_HASH_BYTES,
                "1 GiB hashed bytes",
            )?;
            hash.update(&chunk[..count]);
        }
    }
}

fn charge(
    used: &mut u64,
    amount: u64,
    maximum: u64,
    label: &'static str,
) -> Result<(), CheckpointError> {
    if amount > maximum.saturating_sub(*used) {
        return Err(CheckpointError::OperationLimit(label));
    }
    *used += amount;
    Ok(())
}

pub(super) fn read_metadata(path: &std::path::Path) -> Result<Vec<u8>, CheckpointError> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_METADATA_BYTES as u64 {
        return Err(CheckpointError::OperationLimit("32 MiB metadata"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(CheckpointError::OperationLimit("32 MiB metadata"));
    }
    Ok(bytes)
}

/// Serialize through a bounded writer so rejection precedes a large allocation.
pub(super) fn serialize_metadata(
    value: &impl serde::Serialize,
    pretty: bool,
) -> Result<Vec<u8>, CheckpointError> {
    struct Output {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_METADATA_BYTES.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::Error::other("checkpoint metadata limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output {
        bytes: Vec::new(),
        exceeded: false,
    };
    let result = if pretty {
        serde_json::to_writer_pretty(&mut output, value)
    } else {
        serde_json::to_writer(&mut output, value)
    };
    if output.exceeded {
        return Err(CheckpointError::OperationLimit("32 MiB metadata"));
    }
    result?;
    Ok(output.bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn limits_are_aggregate_and_rejections_do_not_wrap() {
        let mut op = CheckpointOperation {
            capture_bytes: MAX_CAPTURE_BYTES - 1,
            ..CheckpointOperation::default()
        };
        op.capture(1).expect("last byte");
        assert!(matches!(
            op.capture(1),
            Err(CheckpointError::OperationLimit(_))
        ));
        op.paths = MAX_PATHS - 1;
        op.path("last").expect("last path");
        assert!(op.path("overflow").is_err());
        let mut op = CheckpointOperation::default();
        assert!(op.path(&"a/".repeat(MAX_DEPTH)).is_err());
        op.hash_bytes = MAX_HASH_BYTES;
        assert!(op.hash(&b"x"[..]).is_err());
    }

    #[test]
    fn oversized_metadata_is_rejected_before_loading_it() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("manifest.json");
        std::fs::File::create(&path)
            .expect("create")
            .set_len(MAX_METADATA_BYTES as u64 + 1)
            .expect("sparse metadata");
        assert!(matches!(
            read_metadata(&path),
            Err(CheckpointError::OperationLimit("32 MiB metadata"))
        ));
    }

    #[test]
    fn cancellation_and_expiry_stop_before_reading() {
        struct NeverRead;
        impl Read for NeverRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("read after cancellation")
            }
        }
        let mut op = CheckpointOperation::default();
        op.cancellation().cancel();
        assert!(matches!(
            op.hash(NeverRead),
            Err(CheckpointError::Cancelled)
        ));
        let mut op = CheckpointOperation {
            deadline: Instant::now(),
            ..CheckpointOperation::default()
        };
        assert!(matches!(
            op.hash(NeverRead),
            Err(CheckpointError::OperationLimit(_))
        ));
    }
}
