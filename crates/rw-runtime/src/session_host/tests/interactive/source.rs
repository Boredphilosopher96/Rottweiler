//! 10,240,000 bytes of real canonical text, seeded outside the interactive timer in bounded batches.
use rw_store::session::SessionEventLog;
use rw_types::{
    Block, Cost, EngineEvent, EventMeta, Role, SequenceId, SessionId, Turn, TurnId, TurnMeta,
    TurnStatus, Usage, conversation_input::InputSelection,
};
use std::path::Path;
pub(super) const SESSION: &str = "joined-interactive";
pub(super) const CONVERSATIONS: u64 = 5_000;
pub(super) const TEXT_BYTES: usize = 1024;
pub(super) fn seed(storage: &Path) -> serde_json::Value {
    let mut journal = SessionEventLog::open(storage, SESSION).expect("seed canonical journal");
    let mut sequence = journal.next_sequence();
    let mut body_bytes = 0;
    let mut first_source = None;
    for turn in 1..=CONVERSATIONS {
        let mut batch = Vec::with_capacity(6);
        let mut meta = || {
            let value = EventMeta {
                protocol_version: rw_types::PROTOCOL_VERSION,
                session_id: SessionId(SESSION.into()),
                sequence_id: SequenceId(sequence),
                emitted_at: "2026-09-12T00:00:00Z".into(),
                caused_by: None,
            };
            sequence += 1;
            value
        };
        let turn_id = TurnId(turn.to_string());
        batch.push(EngineEvent::TurnStarted {
            meta: meta(),
            turn_id: turn_id.clone(),
        });
        let text = format!("historical input {turn:05}: ")
            .chars()
            .chain(std::iter::repeat('u'))
            .take(TEXT_BYTES)
            .collect::<String>();
        body_bytes += text.len();
        let accepted = meta();
        let source = accepted.sequence_id;
        batch.push(EngineEvent::UserMessageAccepted {
            meta: accepted,
            agent_turn: turn,
            content: text,
            attachments: vec![],
        });
        let input = meta();
        first_source.get_or_insert(input.sequence_id.0.to_string());
        batch.push(EngineEvent::ConversationInputCommitted {
            meta: input,
            agent_turn: turn,
            accepted_source: source,
            selection: InputSelection::Accepted {},
        });
        let text = format!("# Historical result {turn:05}\n\n")
            .chars()
            .chain(std::iter::repeat('a'))
            .take(TEXT_BYTES)
            .collect::<String>();
        body_bytes += text.len();
        batch.push(EngineEvent::ConversationTurnCommitted {
            meta: meta(),
            agent_turn: turn,
            turn: Turn {
                role: Role::Assistant,
                blocks: vec![Block::Text { text }],
                meta: TurnMeta::default(),
            },
        });
        batch.push(EngineEvent::TurnFinished {
            meta: meta(),
            turn_id,
            status: TurnStatus::Completed,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Unavailable {
                reason: "interactive fixture source".into(),
            },
        });
        journal.append_batch(batch).expect("durable seed batch");
    }
    assert_eq!(
        body_bytes,
        usize::try_from(CONVERSATIONS).expect("bounded conversation count") * 2 * TEXT_BYTES
    );
    // Transcript history stays canonical and searchable; provider context starts
    // empty, as after the supported explicit StartWithoutContext model transition.
    journal
        .append(EngineEvent::ModelContextCleared {
            meta: EventMeta {
                protocol_version: rw_types::PROTOCOL_VERSION,
                session_id: SessionId(SESSION.into()),
                sequence_id: SequenceId(sequence),
                emitted_at: "2026-09-12T00:00:00Z".into(),
                caused_by: None,
            },
            strategy: rw_types::ModelContextTransfer::StartWithoutContext,
        })
        .expect("durable provider context boundary");
    let view = journal.read_view();
    serde_json::json!({"conversations":CONVERSATIONS,"conversation_items":CONVERSATIONS*2,
        "text_bytes":body_bytes,"seed_timed":false,"provider_context_reset":true,"source_through":view.last_sequence(),
        "source_digest":view.prefix_identity().digest,"first_source":first_source})
}
