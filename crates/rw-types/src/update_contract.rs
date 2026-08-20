//! Shared limits for the signed update protocol.

/// Largest compressed release artifact that may be signed, verified, or downloaded.
pub const MAX_UPDATE_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
