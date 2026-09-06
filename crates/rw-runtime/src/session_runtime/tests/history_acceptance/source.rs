//! Deterministic, bounded mixed histories; expected source anchors are independent of projection.
use rw_store::session::SessionEventLog;
use rw_types::{
    AttachmentData, Block, Cost, EngineEvent, EventMeta, ModeId, Role, SequenceId, SessionId,
    StoredAttachment, ToolCallId, ToolInvocationId, ToolOutput, Turn, TurnId, TurnMeta, TurnStatus,
    Usage, conversation_input::InputSelection,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(super) const SESSION: &str = "history-acceptance";
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Anchor {
    pub logical_turn: u64,
    pub agent_turn: u64,
    pub accepted: SequenceId,
    pub committed: SequenceId,
    pub text: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Expected {
    pub conversations: u64,
    pub ended_attempts: u64,
    pub next_sequence: u64,
    pub digest: [u8; 32],
    pub journal_bytes: u64,
    pub input_body_bytes: u64,
    pub anchors: Vec<Anchor>,
}
struct Seed {
    log: SessionEventLog,
    sequence: u64,
    turn: u64,
    input_body_bytes: u64,
}
impl Seed {
    fn push(&mut self, batch: &mut Vec<EngineEvent>, event: impl FnOnce(EventMeta) -> EngineEvent) {
        batch.push(event(EventMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            session_id: SessionId(SESSION.into()),
            sequence_id: SequenceId(self.sequence),
            emitted_at: "2026-09-05T00:00:00.000Z".into(),
            caused_by: None,
        }));
        self.sequence += 1;
    }
    fn begin(&mut self, batch: &mut Vec<EngineEvent>) {
        self.turn += 1;
        let turn_id = TurnId(self.turn.to_string());
        self.push(batch, |meta| EngineEvent::TurnStarted { meta, turn_id });
    }
    fn end(&mut self, batch: &mut Vec<EngineEvent>, status: TurnStatus) {
        let turn_id = TurnId(self.turn.to_string());
        self.push(batch, |meta| EngineEvent::TurnFinished {
            meta,
            turn_id,
            status,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Unavailable {
                reason: "deterministic history acceptance".into(),
            },
        });
    }
    fn conversation(&mut self, batch: &mut Vec<EngineEvent>, role: Role, blocks: Vec<Block>) {
        let agent_turn = self.turn;
        self.push(batch, |meta| EngineEvent::ConversationTurnCommitted {
            meta,
            agent_turn,
            turn: Turn {
                role,
                blocks,
                meta: TurnMeta::default(),
            },
        });
    }
    fn input(&mut self, batch: &mut Vec<EngineEvent>, logical: u64) -> Anchor {
        self.begin(batch);
        let original = format!("input {logical}: {}", "text ".repeat(200));
        let attachment = format!("attachment {logical}: {}", "source ".repeat(128));
        self.input_body_bytes += (original.len() + attachment.len()) as u64;
        let accepted = SequenceId(self.sequence);
        let agent_turn = self.turn;
        let attachment = StoredAttachment {
            name: "evidence.txt".into(),
            source_path: None,
            media_type: "text/plain".into(),
            content_hash: blake3::hash(attachment.as_bytes()).to_hex().to_string(),
            byte_len: attachment.len() as u64,
            data: AttachmentData::Text {
                content: attachment,
            },
        };
        let content = original.clone();
        self.push(batch, |meta| EngineEvent::UserMessageAccepted {
            meta,
            agent_turn,
            content,
            attachments: vec![attachment],
        });
        if logical.is_multiple_of(10) {
            self.push(batch, |meta| EngineEvent::UserMessageRetained {
                meta,
                accepted_source: accepted,
            });
            self.end(batch, TurnStatus::Interrupted);
            self.begin(batch);
        }
        let text = if logical.is_multiple_of(3) {
            format!("hook-selected {original}")
        } else {
            original.clone()
        };
        let selection = if text == original {
            InputSelection::Accepted {}
        } else {
            InputSelection::Transformed { text: text.clone() }
        };
        let committed = SequenceId(self.sequence);
        let agent_turn = self.turn;
        self.push(batch, |meta| EngineEvent::ConversationInputCommitted {
            meta,
            agent_turn,
            accepted_source: accepted,
            selection,
        });
        Anchor {
            logical_turn: logical,
            agent_turn,
            accepted,
            committed,
            text,
        }
    }
    fn tool_and_answer(&mut self, batch: &mut Vec<EngineEvent>, logical: u64) {
        let id = ToolCallId("reusable-provider-id".into());
        let invocation = ToolInvocationId(format!("invocation-{logical}"));
        let args = serde_json::json!({"path":"evidence.txt"});
        self.conversation(
            batch,
            Role::Assistant,
            vec![Block::ToolCall {
                id: id.clone(),
                name: "read".into(),
                args: args.clone(),
            }],
        );
        let turn_id = TurnId(self.turn.to_string());
        self.push(batch, |meta| EngineEvent::ToolCallStarted {
            meta,
            turn_id,
            tool_call_id: id.clone(),
            invocation_id: invocation.clone(),
            name: "read".into(),
            args,
            call_index: 0,
        });
        let output = ToolOutput::Text {
            text: format!("result {logical}: {}", "output ".repeat(128)),
        };
        let turn_id = TurnId(self.turn.to_string());
        self.push(batch, |meta| EngineEvent::ToolCallFinished {
            meta,
            turn_id,
            tool_call_id: id.clone(),
            invocation_id: invocation,
            output: output.clone(),
            is_error: false,
            call_index: 0,
            presentation: None,
        });
        self.conversation(
            batch,
            Role::Tool,
            vec![Block::ToolResult {
                id,
                output,
                is_error: false,
            }],
        );
        self.conversation(
            batch,
            Role::Assistant,
            vec![Block::Text {
                text: format!("answer {logical}: {}", "answer ".repeat(64)),
            }],
        );
        self.end(batch, TurnStatus::Completed);
    }
}

pub(super) fn seed(storage: &Path, conversations: u64) -> Expected {
    let mut seed = Seed {
        log: SessionEventLog::open(storage, SESSION).expect("source"),
        sequence: 0,
        turn: 0,
        input_body_bytes: 0,
    };
    let modes = rw_ext::ModeRegistry::builtins().expect("modes");
    let fingerprint = modes.get("plan").expect("plan").semantic_fingerprint();
    let mut batch = Vec::with_capacity(16);
    seed.push(&mut batch, |meta| EngineEvent::ModeChanged {
        meta,
        mode: ModeId("plan".into()),
        definition_fingerprint: fingerprint,
    });
    seed.push(&mut batch, |meta| EngineEvent::SessionTitleUpdated {
        meta,
        title: "Mixed source history".into(),
        usage: None,
        cost: None,
    });
    seed.log.append_batch(batch.drain(..)).expect("header");
    let mut anchors = Vec::with_capacity(3);
    for logical in 1..=conversations {
        let anchor = seed.input(&mut batch, logical);
        seed.tool_and_answer(&mut batch, logical);
        assert!(batch.len() <= 16, "seed retains one turn only");
        seed.log
            .append_batch(batch.drain(..))
            .expect("one complete turn");
        if [1, conversations / 2 + 1, conversations].contains(&logical) {
            anchors.push(anchor);
        }
    }
    seed.push(&mut batch, |meta| EngineEvent::PlanSubmitted {
        meta,
        artifact: super::super::dormant_controls::artifact(),
    });
    seed.push(&mut batch, |meta| EngineEvent::MessageQueued {
        meta,
        position: 0,
        content: "remain queued during selected reattach".into(),
        attachments: vec![],
    });
    seed.log.append_batch(batch).expect("pending controls");
    let view = seed.log.read_view();
    let tail = view
        .page::<EngineEvent>(
            view.last_sequence(),
            rw_store::session::SessionEventPageLimits::default(),
        )
        .expect("source size");
    Expected {
        conversations,
        ended_attempts: seed.turn,
        next_sequence: seed.sequence,
        digest: view.prefix_identity().digest,
        journal_bytes: tail.total_bytes,
        input_body_bytes: seed.input_body_bytes,
        anchors,
    }
}
