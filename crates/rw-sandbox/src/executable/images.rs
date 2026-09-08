//! Application-owned reusable executable bytes. Source receipts remain independent.
use super::{ApprovedExecutable, ExecutableArtifactIdentity, ExecutableBacking, identity, invalid};
use crate::SandboxError;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_IMAGES: usize = 32;
const MAX_ORIGINS: usize = 64;
const MAX_ORIGIN_PATH_BYTES: usize = 4_096;
const CAPTURE_WAIT: Duration = Duration::from_secs(30);

/// Limits cover live images, including evicted images still held by processes.
/// Metadata is separately bounded by 32 entries and 64 origin receipts of at
/// most 4096 path bytes each; file content is never read into a heap buffer.
#[derive(Clone, Copy, Debug)]
pub struct ExecutableImageLimits {
    images: usize,
    bytes: u64,
}
impl Default for ExecutableImageLimits {
    fn default() -> Self {
        Self {
            images: MAX_IMAGES,
            bytes: super::MAX_EXECUTABLE_BYTES,
        }
    }
}
impl ExecutableImageLimits {
    /// Constructs smaller application limits without widening native admission.
    ///
    /// # Errors
    /// Rejects zero or limits above 32 images / 256 MiB.
    pub fn new(images: usize, bytes: u64) -> Result<Self, SandboxError> {
        if images == 0 || images > MAX_IMAGES || bytes == 0 || bytes > super::MAX_EXECUTABLE_BYTES {
            return Err(invalid("invalid approved executable image limits"));
        }
        Ok(Self { images, bytes })
    }
}

#[derive(Debug, Default)]
struct Accounting {
    images: AtomicUsize,
    bytes: AtomicU64,
    origins: AtomicUsize,
}
#[derive(Debug)]
pub(super) struct ImageCredit {
    accounting: Arc<Accounting>,
    bytes: u64,
}
impl Drop for ImageCredit {
    fn drop(&mut self) {
        self.accounting
            .bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
        self.accounting.images.fetch_sub(1, Ordering::AcqRel);
    }
}
#[derive(Debug)]
pub(super) struct OriginCredit(Arc<Accounting>);
impl Drop for OriginCredit {
    fn drop(&mut self) {
        self.0.origins.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    bytes: u64,
    sha256: [u8; 64],
}
impl Key {
    fn new(receipt: &ExecutableArtifactIdentity) -> Result<Self, SandboxError> {
        Ok(Self {
            bytes: receipt.bytes,
            sha256: receipt.sha256.as_bytes().try_into().map_err(invalid)?,
        })
    }
}
struct Entry {
    key: Key,
    backing: Option<Arc<ExecutableBacking>>,
}
enum Reservation {
    Admitted(ImageCredit),
    Retire(Entry),
}
struct State {
    closed: bool,
    entries: Vec<Entry>,
}

/// One explicit application owner; construction starts no work and touches no files.
/// Every acquisition verifies its own source descriptor and complete digest.
/// Copies and source hashing run outside the short publication lock. Concurrent
/// misses for identical bytes share one copy, with at most 64 bounded waiters or
/// live origin bindings. Captures must run in a physically owned blocking task.
/// Dropping this owner retires cache entries; launch guards retain their bytes
/// and credit independently until the last physical owner is destroyed.
pub struct ApprovedExecutableImages {
    limits: ExecutableImageLimits,
    accounting: Arc<Accounting>,
    state: Mutex<State>,
    changed: Condvar,
}
impl Default for ApprovedExecutableImages {
    fn default() -> Self {
        Self::new(ExecutableImageLimits::default())
    }
}
impl ApprovedExecutableImages {
    /// Creates a bounded, inert application owner.
    #[must_use]
    pub fn new(limits: ExecutableImageLimits) -> Self {
        Self {
            limits,
            accounting: Arc::new(Accounting::default()),
            state: Mutex::new(State {
                closed: false,
                entries: Vec::with_capacity(limits.images),
            }),
            changed: Condvar::new(),
        }
    }

    /// Verifies the exact original artifact, then shares or creates its immutable image.
    ///
    /// # Errors
    /// Rejects changed source authority, exhausted physical capacity, closure,
    /// failed copies, or an identical in-flight capture exceeding 30 seconds.
    pub fn acquire(
        &self,
        receipt: &ExecutableArtifactIdentity,
    ) -> Result<ApprovedExecutable, SandboxError> {
        let origin = {
            let state = self.state.lock().map_err(invalid)?;
            if state.closed {
                return Err(invalid("approved executable images are closed"));
            }
            self.origin(receipt)?
        };
        let source = ApprovedExecutable::source(receipt)?;
        let key = Key::new(receipt)?;
        let deadline = Instant::now() + CAPTURE_WAIT;
        let mut state = self.state.lock().map_err(invalid)?;
        loop {
            if state.closed {
                return Err(invalid("approved executable images are closed"));
            }
            if let Some(index) = state.entries.iter().position(|entry| entry.key == key) {
                if let Some(backing) = &state.entries[index].backing {
                    let backing = Arc::clone(backing);
                    let entry = state.entries.remove(index);
                    state.entries.push(entry);
                    drop(state);
                    identity::verify_copy(receipt, &source, &mut std::io::sink())?;
                    // Closure may race source verification. The origin and image
                    // remain charged; no new binding is published after the fence.
                    let state = self.state.lock().map_err(invalid)?;
                    if state.closed {
                        return Err(invalid("approved executable images are closed"));
                    }
                    return Ok(ApprovedExecutable::bind(receipt, backing, Some(origin)));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(invalid("approved executable capture wait expired"));
                }
                state = self
                    .changed
                    .wait_timeout(state, remaining)
                    .map_err(invalid)?
                    .0;
                continue;
            }
            let credit = match self.reserve(&mut state, receipt.bytes)? {
                Reservation::Admitted(credit) => credit,
                Reservation::Retire(entry) => {
                    drop(state);
                    drop(entry);
                    state = self.state.lock().map_err(invalid)?;
                    continue;
                }
            };
            state.entries.push(Entry { key, backing: None });
            drop(state);
            let pending = Pending { owner: self, key };
            let backing = ApprovedExecutable::snapshot_backing(receipt, &source, Some(credit))?;
            let mut state = self.state.lock().map_err(invalid)?;
            if state.closed {
                return Err(invalid("approved executable images are closed"));
            }
            let entry = state
                .entries
                .iter_mut()
                .find(|entry| entry.key == key)
                .ok_or_else(|| invalid("approved executable capture publication is absent"))?;
            entry.backing = Some(Arc::clone(&backing));
            let approved = ApprovedExecutable::bind(receipt, backing, Some(origin));
            drop(state);
            drop(pending);
            return Ok(approved);
        }
    }

    fn origin(&self, receipt: &ExecutableArtifactIdentity) -> Result<OriginCredit, SandboxError> {
        if receipt.executable.as_os_str().len() > MAX_ORIGIN_PATH_BYTES {
            return Err(invalid(
                "approved executable source path exceeds 4096 bytes",
            ));
        }
        self.accounting
            .origins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_ORIGINS).then_some(count + 1)
            })
            .map_err(|_| invalid("approved executable origin capacity is exhausted"))?;
        Ok(OriginCredit(Arc::clone(&self.accounting)))
    }

    fn reserve(&self, state: &mut State, bytes: u64) -> Result<Reservation, SandboxError> {
        if bytes > self.limits.bytes {
            return Err(invalid(
                "approved executable exceeds application image byte capacity",
            ));
        }
        let owned_bytes = self.accounting.bytes.load(Ordering::Acquire);
        let owned_images = self.accounting.images.load(Ordering::Acquire);
        if owned_images < self.limits.images
            && state.entries.len() < self.limits.images
            && bytes <= self.limits.bytes.saturating_sub(owned_bytes)
        {
            self.accounting.bytes.fetch_add(bytes, Ordering::AcqRel);
            self.accounting.images.fetch_add(1, Ordering::AcqRel);
            return Ok(Reservation::Admitted(ImageCredit {
                accounting: Arc::clone(&self.accounting),
                bytes,
            }));
        }
        let index = state
            .entries
            .iter()
            .position(|entry| {
                entry
                    .backing
                    .as_ref()
                    .is_some_and(|backing| Arc::strong_count(backing) == 1)
            })
            .ok_or_else(|| {
                invalid(
                    "approved executable image capacity remains held by live processes or captures",
                )
            })?;
        Ok(Reservation::Retire(state.entries.remove(index)))
    }

    /// Stops new captures without retiring files on the calling thread.
    ///
    /// # Errors
    /// Rejects a poisoned publication owner.
    pub fn fence(&self) -> Result<(), SandboxError> {
        self.state.lock().map_err(invalid)?.closed = true;
        self.changed.notify_all();
        Ok(())
    }

    /// Fences an unused owner and proves it has no physical retirement work.
    ///
    /// # Errors
    /// Rejects a poisoned publication owner.
    pub fn close_if_empty(&self) -> Result<bool, SandboxError> {
        let mut state = self.state.lock().map_err(invalid)?;
        state.closed = true;
        self.changed.notify_all();
        Ok(state.entries.is_empty()
            && self.accounting.origins.load(Ordering::Acquire) == 0
            && self.accounting.images.load(Ordering::Acquire) == 0)
    }

    /// Fences captures and releases reusable entries after plugin settlement.
    /// Retry after outstanding physical owners retire; success proves every
    /// image and origin owned by this application has been destroyed.
    ///
    /// # Errors
    /// Returns explicit unsettled ownership while a capture, launch, or approved
    /// executable remains alive. Closure never invalidates those live bytes.
    pub fn close(&self) -> Result<(), SandboxError> {
        let mut state = self.state.lock().map_err(invalid)?;
        state.closed = true;
        let retired = std::mem::take(&mut state.entries);
        drop(state);
        self.changed.notify_all();
        drop(retired);
        if self.accounting.origins.load(Ordering::Acquire) != 0
            || self.accounting.images.load(Ordering::Acquire) != 0
        {
            return Err(invalid(
                "approved executable images remain physically owned after closure",
            ));
        }
        Ok(())
    }
}
struct Pending<'a> {
    owner: &'a ApprovedExecutableImages,
    key: Key,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = state
            .entries
            .iter()
            .position(|entry| entry.key == self.key && entry.backing.is_none())
        {
            state.entries.remove(index);
        }
        self.owner.changed.notify_all();
    }
}

#[cfg(test)]
pub(super) mod tests;
