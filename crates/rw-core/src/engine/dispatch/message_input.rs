use crate::engine::PreparedUserMessage;
use crate::engine::model::ModelDriver;
use rw_types::Attachment;
use rw_types::AttachmentData;
use rw_types::Block;
use rw_types::ImageRef;
use rw_types::StoredAttachment;
use rw_types::attachment_contract::MAX_ATTACHMENTS_PER_MESSAGE;
use rw_types::attachment_contract::MAX_IMAGE_ATTACHMENT_BYTES;
use rw_types::attachment_contract::MAX_TEXT_ATTACHMENT_BYTES;
use rw_types::attachment_contract::MAX_TOTAL_ATTACHMENT_BYTES;
use std::path::Component;
use std::path::Path;

#[allow(clippy::too_many_lines)]
pub(in crate::engine) fn prepare_user_message(
    content: &str,
    attachments: &[Attachment],
    model_alias: &str,
    model: &dyn ModelDriver,
) -> Result<PreparedUserMessage, String> {
    if attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(format!(
            "at most {MAX_ATTACHMENTS_PER_MESSAGE} attachments are allowed"
        ));
    }
    let mut total_bytes = 0_usize;
    let mut stored_attachments = Vec::with_capacity(attachments.len());
    let mut attachment_blocks = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        if attachment.name.is_empty()
            || attachment.name.len() > 255
            || attachment.name == "."
            || attachment.name == ".."
            || attachment
                .name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err("attachment names must be safe single path components".to_owned());
        }
        if let Some(source_path) = attachment.source_path.as_deref()
            && !is_safe_relative_attachment_path(source_path)
        {
            return Err(
                "attachment source paths must be normalized workspace-relative paths".to_owned(),
            );
        }
        if attachment.media_type.trim() != attachment.media_type
            || attachment.media_type.to_ascii_lowercase() != attachment.media_type
        {
            return Err(
                "attachment media types must be canonical lowercase MIME values".to_owned(),
            );
        }
        let (byte_len, content_hash, block) = match (
            &attachment.data,
            attachment.media_type.as_str(),
        ) {
            (AttachmentData::Text { content }, media_type)
                if media_type.starts_with("text/") || media_type == "application/json" =>
            {
                if content.len() > MAX_TEXT_ATTACHMENT_BYTES {
                    return Err(format!(
                        "text attachment {:?} exceeds {MAX_TEXT_ATTACHMENT_BYTES} bytes",
                        attachment.name
                    ));
                }
                let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
                let label = attachment
                    .source_path
                    .as_deref()
                    .unwrap_or(&attachment.name);
                let text = format!("Attached file {label:?} ({media_type}):\n{content}");
                (content.len(), hash, Block::Text { text })
            }
            (AttachmentData::InlineBase64 { data }, media_type)
                if matches!(
                    media_type,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) =>
            {
                if !model.supports_vision(model_alias) {
                    return Err(format!(
                        "model alias {model_alias:?} does not support image attachments"
                    ));
                }
                let decoded_len = canonical_base64_decoded_len(data).ok_or_else(|| {
                    format!(
                        "image attachment {:?} is not canonical base64",
                        attachment.name
                    )
                })?;
                if decoded_len > MAX_IMAGE_ATTACHMENT_BYTES {
                    return Err(format!(
                        "image attachment {:?} exceeds {MAX_IMAGE_ATTACHMENT_BYTES} decoded bytes",
                        attachment.name
                    ));
                }
                let hash = blake3::hash(data.as_bytes()).to_hex().to_string();
                (
                    decoded_len,
                    hash,
                    Block::Image {
                        media_type: media_type.to_owned(),
                        data: ImageRef::InlineBase64 { data: data.clone() },
                    },
                )
            }
            _ => {
                return Err(format!(
                    "attachment {:?} has unsupported data for media type {:?}",
                    attachment.name, attachment.media_type
                ));
            }
        };
        total_bytes = total_bytes.saturating_add(byte_len);
        if total_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err(format!(
                "attachments exceed the {MAX_TOTAL_ATTACHMENT_BYTES}-byte total limit"
            ));
        }
        stored_attachments.push(StoredAttachment {
            name: attachment.name.clone(),
            source_path: attachment.source_path.clone(),
            media_type: attachment.media_type.clone(),
            content_hash,
            byte_len: u64::try_from(byte_len).unwrap_or(u64::MAX),
        });
        attachment_blocks.push(block);
    }
    if content.is_empty() && attachment_blocks.is_empty() {
        return Err("message content and attachments cannot both be empty".to_owned());
    }
    Ok(PreparedUserMessage {
        content: content.to_owned(),
        stored_attachments,
        attachment_blocks,
    })
}

pub(super) fn is_safe_relative_attachment_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4_096
        && !value.chars().any(char::is_control)
        && !value.contains('\\')
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

pub(super) fn canonical_base64_decoded_len(data: &str) -> Option<usize> {
    if data.is_empty() || !data.len().is_multiple_of(4) || !data.is_ascii() {
        return None;
    }
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    if padding > 2 {
        return None;
    }
    let payload_len = data.len().checked_sub(padding)?;
    if data
        .bytes()
        .take(payload_len)
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')))
        || data.bytes().skip(payload_len).any(|byte| byte != b'=')
    {
        return None;
    }
    data.len()
        .checked_div(4)?
        .checked_mul(3)?
        .checked_sub(padding)
}
