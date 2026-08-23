//! Attachment limits shared by the engine and generated client projections.

/// Maximum number of attachments accepted with one user message.
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 16;
/// Maximum UTF-8 byte length of one text attachment.
pub const MAX_TEXT_ATTACHMENT_BYTES: usize = 1024 * 1024;
/// Maximum decoded byte length of one image attachment.
pub const MAX_IMAGE_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
/// Maximum combined decoded byte length of all attachments on one message.
pub const MAX_TOTAL_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
