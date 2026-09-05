//! Descriptor validation, bounded record decoding, append faults, and durable file I/O.
use super::{EVENT_SCHEMA_VERSION, EventEnvelope, SessionStoreError};
use rw_types::SequenceId;
use serde::de::DeserializeOwned;
#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
#[cfg(not(unix))]
use std::{
    fs,
    fs::OpenOptions,
    io::{Read as _, Seek as _, SeekFrom},
    path::Path,
};
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
};

pub(super) fn truncate_and_sync_event_file(file: &File, len: u64) -> std::io::Result<()> {
    set_event_file_len(file, len)?;
    sync_event_file(file)
}

pub(super) fn write_event_bytes(file: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(fail_after) = take_partial_append_write_fault() {
        file.write_all(&bytes[..bytes.len().min(fail_after)])?;
        return Err(std::io::Error::other(
            "injected partial event-log append failure",
        ));
    }
    file.write_all(bytes)
}

pub(super) fn set_event_file_len(file: &File, len: u64) -> std::io::Result<()> {
    #[cfg(test)]
    if take_append_truncate_fault() {
        return Err(std::io::Error::other("injected event-log rollback failure"));
    }
    file.set_len(len)
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) struct AppendFault {
    partial_write_after: Option<usize>,
    fail_truncate: bool,
}

#[cfg(test)]
thread_local! {
    pub(super) static EVENT_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn run_event_read_hook() {
    let hook = EVENT_READ_HOOK.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
thread_local! {
    static APPEND_FAULT: std::cell::Cell<Option<AppendFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn install_append_fault(
    partial_write_after: usize,
    fail_truncate: bool,
) -> AppendFaultGuard {
    APPEND_FAULT.with(|fault| {
        assert!(fault.get().is_none(), "append fault already installed");
        fault.set(Some(AppendFault {
            partial_write_after: Some(partial_write_after),
            fail_truncate,
        }));
    });
    AppendFaultGuard
}

#[cfg(test)]
pub(super) struct AppendFaultGuard;

#[cfg(test)]
impl Drop for AppendFaultGuard {
    fn drop(&mut self) {
        APPEND_FAULT.with(|fault| fault.set(None));
    }
}

#[cfg(test)]
pub(super) fn take_partial_append_write_fault() -> Option<usize> {
    APPEND_FAULT.with(|fault| {
        let mut state = fault.get()?;
        let fail_after = state.partial_write_after.take();
        fault.set(Some(state));
        fail_after
    })
}

#[cfg(test)]
pub(super) fn take_append_truncate_fault() -> bool {
    APPEND_FAULT.with(|fault| {
        let Some(mut state) = fault.get() else {
            return false;
        };
        let fail = state.fail_truncate;
        state.fail_truncate = false;
        fault.set(Some(state));
        fail
    })
}

#[cfg(unix)]
#[derive(Debug)]
pub(super) struct EventFileSnapshot {
    stat: rustix::fs::Stat,
}

#[cfg(unix)]
impl EventFileSnapshot {
    pub(super) fn len(&self) -> u64 {
        u64::try_from(self.stat.st_size).unwrap_or(u64::MAX)
    }
}

#[cfg(unix)]
pub(super) fn event_file_snapshot(file: &File) -> Result<EventFileSnapshot, SessionStoreError> {
    let stat = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(SessionStoreError::UnsafeEventFileType);
    }
    u64::try_from(stat.st_size).map_err(|_| SessionStoreError::LimitOverflow)?;
    Ok(EventFileSnapshot { stat })
}

#[cfg(unix)]
pub(super) fn verify_event_file_snapshot(
    file: &File,
    before: &EventFileSnapshot,
) -> Result<(), SessionStoreError> {
    let after = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(after.st_mode).is_file()
        || after.st_nlink != 1
        || after.st_dev != before.stat.st_dev
        || after.st_ino != before.stat.st_ino
        || after.st_size != before.stat.st_size
        || after.st_mtime != before.stat.st_mtime
        || after.st_mtime_nsec != before.stat.st_mtime_nsec
        || after.st_ctime != before.stat.st_ctime
        || after.st_ctime_nsec != before.stat.st_ctime_nsec
    {
        return Err(SessionStoreError::EventFileChangedDuringRead);
    }
    Ok(())
}

#[cfg(not(unix))]
#[derive(Debug)]
pub(super) struct EventFileSnapshot {
    len: u64,
    modified: std::time::SystemTime,
}

#[cfg(not(unix))]
impl EventFileSnapshot {
    const fn len(&self) -> u64 {
        self.len
    }
}

#[cfg(not(unix))]
pub(super) fn event_file_snapshot(file: &File) -> Result<EventFileSnapshot, SessionStoreError> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(SessionStoreError::UnsafeEventFileType);
    }
    Ok(EventFileSnapshot {
        len: metadata.len(),
        modified: metadata.modified()?,
    })
}

#[cfg(not(unix))]
pub(super) fn verify_event_file_snapshot(
    file: &File,
    before: &EventFileSnapshot,
) -> Result<(), SessionStoreError> {
    let after = file.metadata()?;
    if !after.file_type().is_file()
        || after.len() != before.len
        || after.modified()? != before.modified
    {
        return Err(SessionStoreError::EventFileChangedDuringRead);
    }
    Ok(())
}

pub(super) fn parse_events_bounded_from_sequence<T: DeserializeOwned>(
    bytes: &[u8],
    max_events: usize,
    first_sequence: u64,
) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
    let mut events = Vec::new();
    for line in BufReader::new(bytes).lines() {
        if events.len() >= max_events {
            return Err(SessionStoreError::EventCountTooLarge { max_events });
        }
        let line = line?;
        if line.is_empty() {
            return Err(SessionStoreError::CorruptEvent("blank JSONL record"));
        }
        let envelope: EventEnvelope<T> = serde_json::from_str(&line)?;
        if envelope.schema_version != EVENT_SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedEventVersion(
                envelope.schema_version,
            ));
        }
        let expected = first_sequence
            .checked_add(
                u64::try_from(events.len()).map_err(|_| SessionStoreError::SequenceOverflow)?,
            )
            .ok_or(SessionStoreError::SequenceOverflow)?;
        if envelope.sequence != SequenceId(expected) {
            return Err(SessionStoreError::CorruptEvent(
                "non-contiguous event sequence",
            ));
        }
        events.push(envelope);
    }
    Ok(events)
}

#[cfg(unix)]
pub(super) fn read_opened_file_bounded(
    file: &File,
    max_bytes: u64,
) -> Result<Vec<u8>, SessionStoreError> {
    let stat = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(SessionStoreError::UnsafeEventFileType);
    }
    let file_bytes = u64::try_from(stat.st_size).map_err(|_| SessionStoreError::LimitOverflow)?;
    if file_bytes > max_bytes {
        return Err(SessionStoreError::EventLogTooLarge { max_bytes });
    }
    let length = usize::try_from(file_bytes).map_err(|_| SessionStoreError::LimitOverflow)?;
    let mut bytes = vec![0_u8; length];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let position = u64::try_from(offset).map_err(|_| SessionStoreError::LimitOverflow)?;
        let read = file.read_at(&mut bytes[offset..], position)?;
        if read == 0 {
            bytes.truncate(offset);
            break;
        }
        offset = offset
            .checked_add(read)
            .ok_or(SessionStoreError::LimitOverflow)?;
    }
    let after = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(after.st_mode).is_file()
        || after.st_nlink != 1
        || after.st_dev != stat.st_dev
        || after.st_ino != stat.st_ino
        || after.st_size != stat.st_size
        || after.st_mtime != stat.st_mtime
        || after.st_mtime_nsec != stat.st_mtime_nsec
        || after.st_ctime != stat.st_ctime
        || after.st_ctime_nsec != stat.st_ctime_nsec
    {
        return Err(SessionStoreError::EventFileChangedDuringRead);
    }
    Ok(bytes)
}

#[cfg(not(unix))]
pub(super) fn read_opened_file_bounded(
    file: &File,
    max_bytes: u64,
) -> Result<Vec<u8>, SessionStoreError> {
    let mut file = file.try_clone()?;
    if file.metadata()?.len() > max_bytes {
        return Err(SessionStoreError::EventLogTooLarge { max_bytes });
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(|_| SessionStoreError::LimitOverflow)? > max_bytes {
        return Err(SessionStoreError::EventLogTooLarge { max_bytes });
    }
    Ok(bytes)
}

#[cfg(unix)]
pub(super) fn open_or_create_directory(
    parent: &File,
    name: &str,
) -> Result<File, SessionStoreError> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
    match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(descriptor) => Ok(File::from(descriptor)),
        Err(rustix::io::Errno::NOENT) => {
            match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::from_bits_truncate(0o700)) {
                Ok(()) => sync_event_file(parent)?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(source) => return Err(std::io::Error::from(source).into()),
            }
            let descriptor = rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty())
                .map_err(std::io::Error::from)?;
            Ok(File::from(descriptor))
        }
        Err(source) => Err(std::io::Error::from(source).into()),
    }
}

#[cfg(not(unix))]
pub(super) fn create_checked_directory_portable(path: &Path) -> Result<(), SessionStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(SessionStoreError::UnsafeSessionDirectory),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(source) => Err(source.into()),
    }
}

#[cfg(unix)]
pub(super) fn sync_event_file(file: &File) -> std::io::Result<()> {
    rustix::fs::fsync(file).map_err(std::io::Error::from)
}

#[cfg(not(unix))]
pub(super) fn sync_event_file(file: &File) -> std::io::Result<()> {
    file.sync_all()
}

pub(super) fn validate_session_id(value: &str) -> Result<(), SessionStoreError> {
    rw_types::SessionId::validate(value).map_err(|_| SessionStoreError::InvalidSessionId)
}
