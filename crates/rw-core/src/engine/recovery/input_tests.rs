#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery,
    tests::{append, catch_up},
};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{AttachmentData, StoredAttachment};

#[test]
fn accepted_attachment_body_survives_before_provider_ir_commit() {
    let directory = tempfile::tempdir().expect("directory");
    let mut journal = SegmentedJournal::open(directory.path(), "canonical").expect("journal");
    let attachment = StoredAttachment {
        name: "source.txt".into(),
        source_path: Some("now-deleted/source.txt".into()),
        media_type: "text/plain".into(),
        content_hash: blake3::hash(b"immutable").to_hex().to_string(),
        byte_len: 9,
        data: AttachmentData::Text {
            content: "immutable".into(),
        },
    };
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::UserMessageAccepted {
                turn: 1,
                content: "inspect".into(),
                attachments: vec![attachment.clone()],
            },
        ],
    );
    let modes = ModeRegistry::builtins().expect("modes");
    let source = journal.read_view();
    let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("recovery");
    catch_up(&mut recovery, &source, &modes);
    drop(recovery);
    let recovery = CanonicalRecovery::open(&source, &modes, None).expect("reopen");
    let bootstrap = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&source)
        .expect("source")
        .bootstrap()
        .expect("bootstrap");
    assert_eq!(
        bootstrap.head.conversation.turns, 0,
        "accepted input is not committed model IR"
    );
    assert_eq!(bootstrap.controls.accepted_messages.len(), 1);
    assert_eq!(
        bootstrap.controls.accepted_messages[0].1.attachments,
        vec![attachment]
    );
}
