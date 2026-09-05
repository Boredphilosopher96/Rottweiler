//! Source-owned limits for bounded in-progress transcript previews.

/// Text and reasoning each retain a UTF-8 prefix; committed IR owns full content.
pub const TRANSCRIPT_TAIL_TEXT_BYTES: usize = 64 * 1024;
/// Each admitted invocation retains a combined-stream display prefix.
pub const TRANSCRIPT_TAIL_TOOL_BYTES: usize = 8 * 1024;
