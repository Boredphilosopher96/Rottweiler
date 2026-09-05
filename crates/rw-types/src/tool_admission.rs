//! Admission ceilings for a session's announced, unresolved tool-call batch.
pub const MAX_PENDING_TOOL_INVOCATIONS: usize = 128;
pub const MAX_PENDING_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_PENDING_TOOL_PREPARED_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TOOL_CALL_ID_BYTES: usize = 1024;
pub const MAX_TOOL_NAME_BYTES: usize = 256;
