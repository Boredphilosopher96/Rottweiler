#![cfg(test)]

use crate::engine::builtin_hook_dispatcher;
use crate::engine::dispatch::prepare_user_message;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::AliasVisionModel;
use crate::engine::tests::fixtures::models::DeferredVisionModel;
use crate::engine::tests::fixtures::support::CanarySecretRedactor;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::protocol_meta;
use rw_tools::ToolRegistry;
use rw_types::Attachment;
use rw_types::AttachmentData;
use rw_types::Block;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::ImageRef;
use rw_types::SessionId;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

#[test]
fn attachment_validation_is_bounded_provider_neutral_and_vision_gated() {
    let text = Attachment {
        name: "notes.txt".to_owned(),
        source_path: Some("docs/KNOWN_CANARY notes with spaces.txt".to_owned()),
        media_type: "text/plain".to_owned(),
        data: AttachmentData::Text {
            content: "bounded KNOWN_CANARY context".to_owned(),
        },
    };
    let prepared = prepare_user_message("inspect KNOWN_CANARY", &[text], "fast", &AliasVisionModel)
        .expect("text attachment")
        .redact(&CanarySecretRedactor)
        .expect("bounded redacted attachment");
    assert_eq!(prepared.stored_attachments.len(), 1);
    assert_eq!(prepared.stored_attachments[0].content_hash.len(), 64);
    assert_eq!(
        prepared.stored_attachments[0].source_path.as_deref(),
        Some("docs/[REDACTED] notes with spaces.txt")
    );
    assert!(matches!(
        &prepared.attachment_blocks[0],
        Block::Text { text }
            if text.contains("docs/[REDACTED] notes with spaces.txt")
                && text.contains("[REDACTED]")
                && !text.contains("KNOWN_CANARY")
    ));
    assert_eq!(prepared.content, "inspect [REDACTED]");

    let image = Attachment {
        name: "screen.png".to_owned(),
        source_path: None,
        media_type: "image/png".to_owned(),
        data: AttachmentData::InlineBase64 {
            data: "iVBORw0KGgo=".to_owned(),
        },
    };
    assert!(
        prepare_user_message(
            "inspect",
            std::slice::from_ref(&image),
            "fast",
            &AliasVisionModel
        )
        .expect_err("non-vision alias must reject before acceptance")
        .contains("does not support image")
    );
    let prepared = prepare_user_message("inspect", &[image], "slow", &AliasVisionModel)
        .expect("vision attachment")
        .redact(&CanarySecretRedactor)
        .expect("bounded redacted attachment");
    assert!(matches!(
        &prepared.attachment_blocks[0],
        Block::Image {
            data: ImageRef::InlineBase64 { data },
            ..
        } if data == "iVBORw0KGgo="
    ));

    let unsafe_name = Attachment {
        name: "../secret.txt".to_owned(),
        source_path: None,
        media_type: "text/plain".to_owned(),
        data: AttachmentData::Text {
            content: "secret".to_owned(),
        },
    };
    assert!(prepare_user_message("inspect", &[unsafe_name], "fast", &AliasVisionModel).is_err());

    let unsafe_source_path = Attachment {
        name: "secret.txt".to_owned(),
        source_path: Some("../secret.txt".to_owned()),
        media_type: "text/plain".to_owned(),
        data: AttachmentData::Text {
            content: "secret".to_owned(),
        },
    };
    assert!(
        prepare_user_message("inspect", &[unsafe_source_path], "fast", &AliasVisionModel)
            .expect_err("traversal source path must fail before acceptance")
            .contains("workspace-relative")
    );
}

#[tokio::test]
async fn first_image_message_prepares_lazy_model_before_vision_validation() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(DeferredVisionModel::default());
    let handle = SessionActor::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach-driver"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach"),
        CommandOutcome::Accepted {}
    );
    assert!(!model.prepared.load(Ordering::Acquire));
    let image = Attachment {
        name: "screen.png".to_owned(),
        source_path: None,
        media_type: "image/png".to_owned(),
        data: AttachmentData::InlineBase64 {
            data: "iVBORw0KGgo=".to_owned(),
        },
    };
    assert_eq!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "first-image"),
                session_id,
                content: "inspect this image".to_owned(),
                attachments: vec![image],
            })
            .await
            .expect("image message"),
        CommandOutcome::Accepted {}
    );
    assert!(model.prepared.load(Ordering::Acquire));
}

#[test]
fn accepted_attachment_source_reconstructs_the_redacted_prepared_message() {
    let input = Attachment {
        name: "notes.txt".into(),
        source_path: Some("docs/notes.txt".into()),
        media_type: "text/plain".into(),
        data: AttachmentData::Text {
            content: "KNOWN_CANARY retained source".into(),
        },
    };
    let prepared = prepare_user_message("inspect", &[input], "fast", &AliasVisionModel)
        .expect("prepare")
        .redact(&CanarySecretRedactor)
        .expect("redact");
    let encoded = serde_json::to_value(&prepared.stored_attachments).expect("source");
    assert!(!encoded.to_string().contains("KNOWN_CANARY"));
    let stored: Vec<rw_types::StoredAttachment> =
        serde_json::from_value(encoded).expect("read durable source");
    let restored = crate::engine::dispatch::recover_user_message(&prepared.content, &stored)
        .expect("recover without provider or filesystem");
    assert_eq!(
        restored.turn(restored.content.clone()),
        prepared.turn(prepared.content.clone())
    );
    assert_eq!(restored.stored_attachments, prepared.stored_attachments);
    let mut invalid = restored.stored_attachments;
    invalid[0].data = AttachmentData::Text {
        content: "different body".into(),
    };
    assert!(crate::engine::dispatch::recover_user_message("inspect", &invalid).is_err());
}

#[test]
fn accepted_attachment_source_requires_its_body() {
    let input = Attachment {
        name: "notes.txt".into(),
        source_path: None,
        media_type: "text/plain".into(),
        data: AttachmentData::Text {
            content: "source".into(),
        },
    };
    let prepared =
        prepare_user_message("inspect", &[input], "fast", &AliasVisionModel).expect("prepare");
    let mut value = serde_json::to_value(&prepared.stored_attachments[0]).expect("source");
    value.as_object_mut().expect("object").remove("data");
    assert!(serde_json::from_value::<rw_types::StoredAttachment>(value).is_err());
}
