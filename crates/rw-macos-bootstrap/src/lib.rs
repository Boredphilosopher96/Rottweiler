//! Audited macOS process authority and running executable identity.
#![deny(unsafe_code)]

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod authority;

/// Clears inherited application authority in a single-threaded worker, then execs.
///
/// This must be called in the dedicated worker entry, before runtime threads or
/// untrusted code start. The thread-count check rejects ordinary running hosts.
/// Immediate exec replaces the ordinary IPC namespace, including queued rights.
/// The immutable OS task-access service remains, subject to Seatbelt task policy.
///
/// # Errors
/// Returns an error if proof fails or exec fails. Success replaces this process.
#[cfg(target_os = "macos")]
#[must_use]
pub fn exec_worker(program: &std::ffi::OsStr, args: &[std::ffi::OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;
    if let Err(error) = authority::clear() {
        return error;
    }
    std::process::Command::new(program).args(args).exec()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod image_identity;

/// Returns the kernel device/inode for this statically linked executable image.
///
/// # Errors
/// Rejects unavailable or inconsistent kernel mapping information.
#[cfg(target_os = "macos")]
pub fn running_image_identity() -> std::io::Result<(u64, u64)> {
    image_identity::running()
}
