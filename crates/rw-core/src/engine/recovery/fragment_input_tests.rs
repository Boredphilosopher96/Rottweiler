#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery, ConversationFragmentCursor, MAX_MATERIALIZED_HISTORY_BYTES,
    MAX_SUMMARY_FRAGMENT_BYTES,
    encoding::serialized_size,
    tests::{append, catch_up, event, terminal},
};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::{SessionEventPageLimits, journal::SegmentedJournal};
use rw_types::{
    AttachmentData, Block, SequenceId, StoredAttachment, conversation_input::InputSelection,
};

fn input_near_record_limit() -> PendingEvent {
    let mut attachments = (0..16)
        .map(|index| StoredAttachment {
            name: format!("source{index}.txt"),
            source_path: Some("\u{200d}".repeat(1365)),
            media_type: "text/plain".into(),
            content_hash: "0".repeat(64),
            byte_len: 0,
            data: AttachmentData::Text {
                content: String::new(),
            },
        })
        .collect::<Vec<_>>();
    let empty = PendingEvent::UserMessageAccepted {
        turn: 1,
        content: String::new(),
        attachments: attachments.clone(),
    };
    let overhead = serialized_size(&event(1, empty)).expect("envelope size") as usize;
    let content =
        "\0".repeat((SessionEventPageLimits::default().max_line_bytes - overhead - 4096) / 6 / 16);
    for attachment in &mut attachments {
        attachment.byte_len = content.len() as u64;
        attachment.content_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        attachment.data = AttachmentData::Text {
            content: content.clone(),
        };
    }
    PendingEvent::UserMessageAccepted {
        turn: 1,
        content: String::new(),
        attachments,
    }
}

#[test]
fn legal_reference_sources_exceeding_window_bytes_fragment_without_body_loss() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let accepted = input_near_record_limit();
    let commit = PendingEvent::ConversationInputCommitted {
        agent_turn: 1,
        accepted_source: SequenceId(1),
        selection: InputSelection::Transformed {
            text: "t".repeat(SessionEventPageLimits::default().max_line_bytes - 4096),
        },
    };
    for value in [&accepted, &commit] {
        assert!(
            serialized_size(&event(2, value.clone())).expect("source encoded")
                < SessionEventPageLimits::default().max_line_bytes as u64
        );
    }
    let expected =
        super::input::resolve_input(&event(2, commit.clone()), &event(1, accepted.clone()))
            .expect("legal accepted input");
    assert!(
        serialized_size(&expected).expect("materialized size") > MAX_MATERIALIZED_HISTORY_BYTES
    );
    let mut expected_hash = blake3::Hasher::new();
    let mut expected_bytes = 0;
    for block in &expected.blocks {
        let encoded = serde_json::to_vec(block).expect("oracle block");
        expected_bytes += encoded.len();
        expected_hash.update(&encoded);
    }
    drop(expected);
    // Each accepted fact has its own bounded transaction, as in the actor.
    // Combining both near-limit records would exceed the batch/segment ceiling.
    for pending in [
        PendingEvent::TurnStarted { turn: 1 },
        accepted,
        commit,
        terminal(1),
    ] {
        append(&mut journal, vec![pending]);
    }
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("owner");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let source = history
        .conversation_fragment_source(0)
        .expect("one source independent of aggregate window");
    let mut cursor = ConversationFragmentCursor::default();
    let mut actual_hash = blake3::Hasher::new();
    let mut actual_bytes = 0;
    loop {
        let part = source
            .fragment(cursor, MAX_SUMMARY_FRAGMENT_BYTES)
            .expect("bounded fragment");
        assert_eq!(part.source.sequence, SequenceId(2));
        let turn = part.turn.expect("body");
        let Block::Text { text } = &turn.blocks[0] else {
            panic!("fragment text")
        };
        let body = text.split_once('\n').expect("framing").1;
        actual_hash.update(body.as_bytes());
        actual_bytes += body.len();
        let Some(next) = part.next else { break };
        cursor = next;
    }
    assert_eq!(actual_bytes, expected_bytes);
    assert_eq!(actual_hash.finalize(), expected_hash.finalize());
}
