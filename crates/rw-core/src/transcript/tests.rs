#![allow(clippy::expect_used)]

use super::*;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{EventMeta, PROTOCOL_VERSION, SessionId, ToolCallId, Turn, TurnMeta};
use tempfile::tempdir;

fn meta(sequence: u64) -> EventMeta {
    EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("semantic".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-04T00:00:00Z".into(),
        caused_by: None,
    }
}
fn turn(sequence: u64, agent_turn: u64, role: Role, blocks: Vec<Block>) -> EngineEvent {
    EngineEvent::ConversationTurnCommitted {
        meta: meta(sequence),
        agent_turn,
        turn: Turn {
            role,
            blocks,
            meta: TurnMeta::default(),
        },
    }
}
fn start(sequence: u64, call_index: u32) -> EngineEvent {
    EngineEvent::ToolCallStarted {
        meta: meta(sequence),
        turn_id: TurnId("1".into()),
        tool_call_id: ToolCallId("reused-provider-id".into()),
        name: "read_file".into(),
        args: serde_json::json!({"path":"example.rs"}),
        call_index,
    }
}
fn finish(sequence: u64, call_index: u32, text: &str) -> EngineEvent {
    EngineEvent::ToolCallFinished {
        meta: meta(sequence),
        turn_id: TurnId("1".into()),
        tool_call_id: ToolCallId("reused-provider-id".into()),
        output: ToolOutput::Text { text: text.into() },
        is_error: false,
        call_index,
    }
}
fn commit(
    event: &EngineEvent,
    journal: &mut SegmentedJournal,
    index: &mut TranscriptIndex,
    state: &mut TranscriptProjectionState,
) {
    journal.append_batch([event]).expect("canonical event");
    let TranscriptEventProjection::Update {
        state: next,
        mutations,
    } = project_transcript_event(event, state, index).expect("project event")
    else {
        panic!("unexpected rewind")
    };
    let before = index.head().expect("head");
    let view = journal.read_view();
    index
        .apply(
            before.prefix,
            &view,
            before.generation,
            &serde_json::to_vec(&next).expect("checkpoint"),
            false,
            &mutations,
        )
        .expect("atomic projection");
    *state = next;
}

#[test]
fn tool_identity_survives_reopen_and_provider_ir_does_not_duplicate_rows() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    let mut index = TranscriptIndex::open(&journal.read_view(), 1).expect("index");
    let mut state = TranscriptProjectionState::default();
    commit(&start(0, 0), &mut journal, &mut index, &mut state);
    commit(
        &turn(
            1,
            1,
            Role::Assistant,
            vec![Block::ToolCall {
                id: ToolCallId("reused-provider-id".into()),
                name: "read_file".into(),
                args: serde_json::json!({}),
            }],
        ),
        &mut journal,
        &mut index,
        &mut state,
    );
    drop(index);
    let mut index =
        TranscriptIndex::open(&journal.read_view(), 1).expect("reopen between call and completion");
    let mut state: TranscriptProjectionState =
        serde_json::from_slice(&index.head().expect("head").state).expect("recover checkpoint");
    commit(
        &finish(2, 0, "first result"),
        &mut journal,
        &mut index,
        &mut state,
    );
    commit(
        &turn(
            3,
            1,
            Role::Tool,
            vec![Block::ToolResult {
                id: ToolCallId("reused-provider-id".into()),
                output: ToolOutput::Text {
                    text: "first result".into(),
                },
                is_error: false,
            }],
        ),
        &mut journal,
        &mut index,
        &mut state,
    );
    commit(&start(4, 1), &mut journal, &mut index, &mut state);
    commit(
        &finish(5, 1, "second result"),
        &mut journal,
        &mut index,
        &mut state,
    );
    let page = index.page(0, 64, 1024 * 1024).expect("semantic page");
    assert_eq!(page.rows.len(), 2);
    assert_eq!(
        page.rows
            .iter()
            .map(|row| (row.source.0, row.revision.0))
            .collect::<Vec<_>>(),
        vec![(0, 2), (4, 5)]
    );
    let TranscriptContent::Tool {
        status: TranscriptToolStatus::Finished { output, .. },
        ..
    } = decode(&page.rows[0]).expect("first tool")
    else {
        panic!("missing tool result");
    };
    assert_eq!(output.text, "first result");
    assert_eq!(output.source.sequence, SequenceId(2));
    assert_eq!(
        index
            .changed_keys(SequenceId(1), 64)
            .expect("late updates")
            .expect("bounded keys")
            .len(),
        2
    );
    assert_eq!(journal.read_view().last_sequence(), Some(SequenceId(5)));
}

#[test]
fn bounded_previews_preserve_utf8_and_complete_source_without_copying_large_values() {
    let text = "🦀\u{0}".repeat(100_000);
    let mut budget = PreviewBudget(513);
    let body = budget.text(
        &text,
        source(SequenceId(7), TranscriptContentSelector::CommandMessage),
    );
    assert!(!body.complete);
    assert!(body.text.len() <= 513);
    assert!(text.starts_with(&body.text));
    let mut budget = PreviewBudget(513);
    let body = budget
        .json(
            &serde_json::json!({"text": text}),
            source(SequenceId(8), TranscriptContentSelector::ToolArguments),
        )
        .expect("capped serialization");
    assert!(!body.complete);
    assert!(body.text.len() <= 513);
    assert_eq!(body.source.sequence, SequenceId(8));
    let mut budget = PreviewBudget(2);
    assert!(
        budget
            .json(
                &serde_json::json!({}),
                source(SequenceId(8), TranscriptContentSelector::ToolArguments)
            )
            .expect("exact limit")
            .complete
    );
}

#[test]
fn conversation_descriptor_and_escaped_byte_limits_are_independent_of_source_size() {
    let root = tempdir().expect("root");
    let journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    let index = TranscriptIndex::open(&journal.read_view(), 1).expect("index");
    let mut blocks = vec![Block::Thinking {
        content: String::new(),
        signature: Some("private continuation".repeat(100_000)),
    }];
    blocks.extend((0..1_000).map(|_| Block::Text {
        text: "\u{0}".repeat(10_000),
    }));
    let event = turn(0, 1, Role::Assistant, blocks);
    let TranscriptEventProjection::Update { mutations, .. } =
        project_transcript_event(&event, &TranscriptProjectionState::default(), &index)
            .expect("bounded projection")
    else {
        panic!("unexpected rewind");
    };
    let TranscriptIndexMutation::Put(row) = &mutations[0] else {
        panic!("missing row");
    };
    assert!(row.payload.len() <= MAX_ROW_BYTES);
    assert!(!String::from_utf8_lossy(&row.payload).contains("private continuation"));
    let TranscriptContent::Conversation {
        blocks,
        omitted_blocks,
        source,
        ..
    } = decode(row).expect("conversation")
    else {
        panic!("wrong row");
    };
    assert_eq!(blocks.len(), TRANSCRIPT_PREVIEW_BLOCKS);
    assert!(omitted_blocks);
    assert_eq!(source.sequence, SequenceId(0));
}

#[test]
fn rewind_is_a_distinct_unpublished_operation_and_invalid_sequences_do_not_advance() {
    let root = tempdir().expect("root");
    let journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    let index = TranscriptIndex::open(&journal.read_view(), 1).expect("index");
    let before = TranscriptProjectionState {
        session_id: Some(SessionId("semantic".into())),
        next_sequence: 10,
        next_ordinal: 4,
        active_turn: Some(2),
    };
    let event = EngineEvent::ConversationRewound {
        meta: meta(10),
        to_agent_turn: 1,
        operation_id: "rewind".into(),
        unrestorable_paths: vec![],
    };
    assert!(matches!(
        project_transcript_event(&event, &before, &index).expect("rewind"),
        TranscriptEventProjection::Rewind {
            target_turn: 1,
            sequence: SequenceId(10)
        }
    ));
    assert_eq!(before.next_sequence, 10);
    assert!(matches!(
        project_transcript_event(&start(9, 0), &before, &index),
        Err(TranscriptProjectionError::Invalid(
            "non-contiguous sequence"
        ))
    ));
    assert_eq!(
        index.head().expect("unpublished head").prefix.next_sequence,
        0
    );
}

#[test]
fn shell_completion_updates_original_row_and_commands_are_not_rewind_owned() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    let mut index = TranscriptIndex::open(&journal.read_view(), 1).expect("index");
    let mut state = TranscriptProjectionState::default();
    commit(
        &EngineEvent::UserShellStateChanged {
            meta: meta(0),
            shell_id: rw_types::ShellId("shell-1".into()),
            command: Some("echo ready".into()),
            active: true,
            status: None,
            captured_output: None,
        },
        &mut journal,
        &mut index,
        &mut state,
    );
    commit(
        &EngineEvent::CommandFinished {
            meta: meta(1),
            name: "status".into(),
            message: "ready".into(),
            unrestorable_paths: vec![],
        },
        &mut journal,
        &mut index,
        &mut state,
    );
    commit(
        &EngineEvent::UserShellStateChanged {
            meta: meta(2),
            shell_id: rw_types::ShellId("shell-1".into()),
            command: None,
            active: false,
            status: Some(0),
            captured_output: Some("ready\n".into()),
        },
        &mut journal,
        &mut index,
        &mut state,
    );
    commit(
        &EngineEvent::UserShellStateChanged {
            meta: meta(3),
            shell_id: rw_types::ShellId("shell-1".into()),
            command: None,
            active: false,
            status: None,
            captured_output: None,
        },
        &mut journal,
        &mut index,
        &mut state,
    );
    let page = index.page(0, 64, 1024 * 1024).expect("page");
    assert_eq!(page.rows.len(), 2);
    assert!(page.rows.iter().all(|row| row.agent_turn.is_none()));
    assert_eq!(
        (page.rows[0].source, page.rows[0].revision),
        (SequenceId(0), SequenceId(3))
    );
    let TranscriptContent::Shell {
        command: Some(command),
        output: Some(output),
        status: Some(0),
        active: false,
    } = decode(&page.rows[0]).expect("shell")
    else {
        panic!("missing shell content");
    };
    assert_eq!(command.text, "echo ready");
    assert_eq!(command.source.sequence, SequenceId(0));
    assert_eq!(output.source.sequence, SequenceId(2));
}

#[test]
fn bounded_projector_resumes_hidden_rewind_after_every_transaction() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    let events = mixed_rewind_events();
    journal.append_batch(&events).expect("canonical events");
    let mut saw_rewind = false;
    let mut completed = false;
    for _ in 0..100 {
        let view = journal.read_view();
        let mut projector = TranscriptProjector::open(&view).expect("reopen projector checkpoint");
        let io = projector.index().io_metrics();
        let progress = projector.advance(&view).expect("one bounded transaction");
        assert!(progress.interpreted_events <= 64);
        assert!(projector.index().io_metrics().bytes_written - io.bytes_written < 4 * 1024 * 1024);
        if progress.rebuilding {
            assert_eq!(progress.applied_next_sequence, 136);
            assert!(matches!(
                projector.index().page(0, 64, 1024 * 1024),
                Err(TranscriptIndexError::Rebuilding)
            ));
            if !saw_rewind {
                saw_rewind = true;
                journal
                    .append_batch([EngineEvent::CommandFinished {
                        meta: meta(139),
                        name: "status".into(),
                        message: "appended during repair".into(),
                        unrestorable_paths: vec![],
                    }])
                    .expect("concurrent append");
            }
        }
        if !progress.has_more {
            let page = projector
                .index()
                .page(0, 64, 1024 * 1024)
                .expect("complete semantic page");
            assert_eq!(page.rows.len(), 55);
            assert_eq!(page.head.prefix, view.prefix_identity());
            assert_eq!(page.head.generation, 1);
            assert!(
                page.rows
                    .iter()
                    .all(|row| row.source.0 < 55 || row.source.0 >= 135)
            );
            assert_eq!(page.rows.last().expect("last row").source, SequenceId(139));
            let tool = page
                .rows
                .iter()
                .find(|row| row.source == SequenceId(2))
                .expect("original tool row");
            assert_eq!(tool.revision, SequenceId(53));
            let TranscriptContent::Tool {
                status: TranscriptToolStatus::Finished { output, .. },
                ..
            } = decode(tool).expect("tool")
            else {
                panic!("tool result absent");
            };
            assert_eq!(output.text, "authoritative complete output");
            assert_eq!(
                projector
                    .index()
                    .at_or_before_source(SequenceId(55))
                    .expect("removed anchor")
                    .expect("replacement")
                    .source,
                SequenceId(52)
            );
            completed = true;
            break;
        }
    }
    assert!(saw_rewind && completed);
}

#[test]
fn typed_projection_rejects_cross_session_events_and_non_durable_progress() {
    let root = tempdir().expect("root");
    let journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    let index = TranscriptIndex::open(&journal.read_view(), 1).expect("index");
    let state = TranscriptProjectionState {
        session_id: Some(SessionId("other".into())),
        ..Default::default()
    };
    assert!(matches!(
        project_transcript_event(&start(0, 0), &state, &index),
        Err(TranscriptProjectionError::Invalid(
            "session/protocol identity"
        ))
    ));
    let event = EngineEvent::SubagentProgress {
        parent_session_id: SessionId("semantic".into()),
        subagent_id: rw_types::SubagentId("child".into()),
        child_session_id: SessionId("child-session".into()),
        child_sequence: Some(SequenceId(0)),
        event: serde_json::json!({"type":"text_delta"}),
    };
    assert!(matches!(
        project_transcript_event(&event, &TranscriptProjectionState::default(), &index),
        Err(TranscriptProjectionError::Invalid("non-durable event"))
    ));
}

fn mixed_rewind_events() -> Vec<EngineEvent> {
    let mut events = vec![
        EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("1".into()),
        },
        turn(
            1,
            1,
            Role::User,
            vec![Block::Text {
                text: "original prompt".into(),
            }],
        ),
        start(2, 0),
    ];
    for sequence in 3..53 {
        events.push(EngineEvent::CommandFinished {
            meta: meta(sequence),
            name: "status".into(),
            message: format!("command {sequence}"),
            unrestorable_paths: vec![],
        });
    }
    events.push(finish(53, 0, "authoritative complete output"));
    events.push(EngineEvent::TurnStarted {
        meta: meta(54),
        turn_id: TurnId("2".into()),
    });
    for sequence in 55..135 {
        events.push(turn(
            sequence,
            2,
            Role::Assistant,
            vec![Block::Text {
                text: format!("removed message {sequence}"),
            }],
        ));
    }
    events.push(EngineEvent::CommandFinished {
        meta: meta(135),
        name: "status".into(),
        message: "survives rewind".into(),
        unrestorable_paths: vec![],
    });
    events.push(EngineEvent::ConversationRewound {
        meta: meta(136),
        to_agent_turn: 1,
        operation_id: "rewind".into(),
        unrestorable_paths: vec![],
    });
    events.push(EngineEvent::TurnStarted {
        meta: meta(137),
        turn_id: TurnId("2".into()),
    });
    events.push(turn(
        138,
        2,
        Role::User,
        vec![Block::Text {
            text: "replacement prompt".into(),
        }],
    ));
    events
}

#[test]
#[ignore = "explicit source/index work qualification; record profile and batch size with timings"]
fn qualify_semantic_projection_10k() {
    use std::time::Instant;
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    for first in (0..10_000).step_by(256) {
        journal
            .append_batch((first..10_000.min(first + 256)).map(|sequence| {
                EngineEvent::CommandFinished {
                    meta: meta(sequence),
                    name: "status".into(),
                    message: format!("item {sequence}: {}", "x".repeat(512)),
                    unrestorable_paths: vec![],
                }
            }))
            .expect("canonical batch");
    }
    let view = journal.read_view();
    let canonical_bytes = view
        .page::<EngineEvent>(
            view.last_sequence(),
            rw_store::session::SessionEventPageLimits::default(),
        )
        .expect("tail metadata")
        .total_bytes;
    assert!(canonical_bytes > 7_000_000);
    let mut projector = TranscriptProjector::open(&view).expect("projector");
    let started = Instant::now();
    let mut batches = 0;
    loop {
        let io = projector.index().io_metrics();
        let progress = projector.advance(&view).expect("bounded catch-up");
        assert!(projector.index().io_metrics().bytes_written - io.bytes_written <= 4 * 1024 * 1024);
        batches += 1;
        if !progress.has_more {
            break;
        }
    }
    assert_eq!(projector.index().head().expect("head").total_rows, 10_000);
    for first in [0, 5_000, 9_936] {
        let page = projector
            .index()
            .page(first, 64, 1024 * 1024)
            .expect("indexed window");
        assert_eq!(page.rows.len(), 64);
        assert_eq!(page.rows[0].source, SequenceId(first));
    }
    let io = projector.index().io_metrics();
    println!(
        "{}",
        serde_json::json!({"profile":if cfg!(debug_assertions) {"debug"} else {"release"}, "events":10_000,"canonical_bytes":canonical_bytes,"batches":batches,"build_ms":started.elapsed().as_secs_f64()*1_000.0,"index_bytes_read":io.bytes_read,"index_bytes_written":io.bytes_written,"index_syncs":io.syncs})
    );
}

#[test]
fn encoded_row_budget_can_stop_mid_page_without_skipping_canonical_events() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    for first in (0..160).step_by(16) {
        journal
            .append_batch((first..first + 16).map(|sequence| {
                turn(
                    sequence,
                    1,
                    Role::User,
                    vec![Block::Text {
                        text: "\u{0}".repeat(20_000),
                    }],
                )
            }))
            .expect("bounded raw batch");
    }
    let view = journal.read_view();
    let mut projector = TranscriptProjector::open(&view).expect("projector");
    let first = projector
        .advance(&view)
        .expect("admitted prefix of raw page");
    assert!(first.interpreted_events > 0 && first.interpreted_events < 64);
    assert_eq!(first.applied_next_sequence, first.interpreted_events as u64);
    while projector
        .advance(&view)
        .expect("continued bounded prefix")
        .has_more
    {}
    let mut ordinal = 0;
    loop {
        let page = projector
            .index()
            .page(ordinal, 64, 1024 * 1024)
            .expect("bounded preview page");
        if page.rows.is_empty() {
            break;
        }
        for row in page.rows {
            assert_eq!(row.source, SequenceId(ordinal));
            assert_eq!(row.ordinal, ordinal);
            ordinal += 1;
        }
    }
    assert_eq!(ordinal, 160);
    assert_eq!(
        projector.index().head().expect("complete head").prefix,
        view.prefix_identity()
    );
}

#[test]
fn projector_finishes_a_tool_from_an_earlier_raw_page_after_reopen() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    let mut events = vec![start(0, 0)];
    events.extend((1..65).map(|sequence| EngineEvent::PluginStatusChanged {
        meta: meta(sequence),
        plugin_id: "probe".into(),
        status: "ready".into(),
    }));
    events.push(finish(65, 0, "complete body from a later page"));
    journal.append_batch(&events).expect("canonical events");
    let view = journal.read_view();
    let mut projector = TranscriptProjector::open(&view).expect("projector");
    let first = projector.advance(&view).expect("first page");
    assert_eq!(first.applied_next_sequence, 64);
    assert!(first.has_more);
    drop(projector);
    let mut projector = TranscriptProjector::open(&view).expect("reopen at page boundary");
    assert!(!projector.advance(&view).expect("later completion").has_more);
    let page = projector
        .index()
        .page(0, 64, 1024 * 1024)
        .expect("one semantic tool");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(
        (page.rows[0].source, page.rows[0].revision),
        (SequenceId(0), SequenceId(65))
    );
    let TranscriptContent::Tool {
        status: TranscriptToolStatus::Finished { output, .. },
        ..
    } = decode(&page.rows[0]).expect("tool")
    else {
        panic!("missing complete output");
    };
    assert_eq!(output.text, "complete body from a later page");
}

#[test]
fn full_content_moves_text_and_reads_utf8_chunks_without_body_copies() {
    let text = "🦀source\n".repeat(50_000);
    let pointer = text.as_ptr();
    let bytes = text.len();
    let document = TranscriptDocument::from_event(
        EngineEvent::CommandFinished {
            meta: meta(0),
            name: "probe".into(),
            message: text,
            unrestorable_paths: vec![],
        },
        &source(SequenceId(0), TranscriptContentSelector::CommandMessage),
        bytes,
    )
    .expect("move body into owner");
    assert_eq!(document.total_bytes(), bytes);
    assert_eq!(document.retained_bytes(), bytes);
    let first = document.chunk(0, 7).expect("UTF-8 slice");
    assert_eq!(first.text.as_ptr(), pointer);
    assert_eq!(first.text, "🦀sou");
    assert_eq!(first.next_offset, Some(7));
    assert!(document.chunk(1, 1024).is_err());
    assert!(document.chunk(0, 3).is_err());
    assert!(document.chunk(bytes + 1, 1024).is_err());
    assert!(document.chunk(0, 0).is_err());
    let mut position = 0;
    loop {
        let chunk = document
            .chunk(position, 8193)
            .expect("bounded continuation");
        assert!(chunk.text.len() <= 8193);
        assert_eq!(chunk.text.as_ptr(), pointer.wrapping_add(position));
        if let Some(next) = chunk.next_offset {
            assert!(next > position);
            position = next;
        } else {
            assert_eq!(position + chunk.text.len(), bytes);
            break;
        }
    }
    assert_eq!(document.chunk(bytes, 1024).expect("EOF").text, "");
}

#[test]
fn full_conversation_has_no_reasoning_signature_or_duplicate_tool_ir() {
    let event = turn(
        7,
        1,
        Role::Assistant,
        vec![
            Block::Thinking {
                content: "visible thought".into(),
                signature: Some("PRIVATE-CONTINUATION".into()),
            },
            Block::ToolCall {
                id: ToolCallId("tool".into()),
                name: "read".into(),
                args: serde_json::json!({"hidden":true}),
            },
            Block::Text {
                text: "visible answer".into(),
            },
        ],
    );
    let document = TranscriptDocument::from_event(
        event.clone(),
        &source(SequenceId(7), TranscriptContentSelector::Conversation),
        4096,
    )
    .expect("display document");
    let body = document.chunk(0, 4096).expect("complete JSON");
    let value: serde_json::Value = serde_json::from_str(body.text).expect("complete JSON syntax");
    assert_eq!(value["blocks"].as_array().expect("display blocks").len(), 2);
    assert_eq!(value["blocks"][0]["content"], "visible thought");
    assert!(!body.text.contains("PRIVATE-CONTINUATION"));
    assert!(!body.text.contains("signature"));
    assert!(!body.text.contains("hidden"));
    let thought = TranscriptDocument::from_event(
        event.clone(),
        &source(
            SequenceId(7),
            TranscriptContentSelector::ConversationBlock { index: 0 },
        ),
        4096,
    )
    .expect("reasoning text");
    assert_eq!(
        thought.chunk(0, 4096).expect("thought").text,
        "visible thought"
    );
    assert!(
        TranscriptDocument::from_event(
            event.clone(),
            &source(
                SequenceId(7),
                TranscriptContentSelector::ConversationBlock { index: 1 }
            ),
            4096
        )
        .is_err()
    );
    assert!(
        TranscriptDocument::from_event(
            event.clone(),
            &source(
                SequenceId(7),
                TranscriptContentSelector::ConversationBlock { index: 99 }
            ),
            4096
        )
        .is_err()
    );
    assert!(
        TranscriptDocument::from_event(
            event.clone(),
            &source(SequenceId(8), TranscriptContentSelector::Conversation),
            4096
        )
        .is_err()
    );
    assert!(
        TranscriptDocument::from_event(
            event,
            &source(SequenceId(7), TranscriptContentSelector::ToolOutput),
            4096
        )
        .is_err()
    );
}

#[test]
fn full_structured_content_is_serialized_once_with_an_allocation_ceiling() {
    let mut event = start(0, 0);
    if let EngineEvent::ToolCallStarted { args, .. } = &mut event {
        *args = serde_json::json!({"escaped":"\u{0}".repeat(8192)});
    }
    let reference = source(SequenceId(0), TranscriptContentSelector::ToolArguments);
    assert!(TranscriptDocument::from_event(event.clone(), &reference, 8192).is_err());
    let document =
        TranscriptDocument::from_event(event, &reference, 65536).expect("bounded JSON document");
    assert_eq!(document.format(), TranscriptPreviewFormat::Json);
    assert!(document.retained_bytes() <= 65536);
    let mut result = String::new();
    let mut offset = 0;
    loop {
        let chunk = document
            .chunk(offset, 113)
            .expect("small JSON continuation");
        result.push_str(chunk.text);
        if let Some(next) = chunk.next_offset {
            offset = next;
        } else {
            break;
        }
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result).expect("reassembled JSON")["escaped"],
        "\u{0}".repeat(8192)
    );
}

#[test]
fn child_completion_retains_one_row_and_authoritative_full_result() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    let mut index = TranscriptIndex::open(&journal.read_view(), 1).expect("index");
    let mut state = TranscriptProjectionState::default();
    let id = rw_types::SubagentId("child-1".into());
    let child_session = SessionId("child-session".into());
    commit(
        &EngineEvent::SubagentSpawned {
            meta: meta(0),
            subagent_id: id.clone(),
            child_session_id: child_session.clone(),
            task: "inspect workspace".into(),
        },
        &mut journal,
        &mut index,
        &mut state,
    );
    let event = EngineEvent::SubagentFinished {
        meta: meta(1),
        subagent_id: id.clone(),
        result: rw_types::SubagentResult {
            subagent_id: id,
            session_id: child_session,
            status: rw_types::SubagentStatus::Completed,
            final_text: "complete output ".repeat(1000),
            touched_files: vec![],
            diff_artifact: None,
            usage: rw_types::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: rw_types::Cost::Unavailable {
                reason: "fixture".into(),
            },
            turns: 1,
            duration_millis: 1,
        },
    };
    commit(&event, &mut journal, &mut index, &mut state);
    let page = index.page(0, 64, 1024 * 1024).expect("page");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(
        (page.rows[0].source, page.rows[0].revision),
        (SequenceId(0), SequenceId(1))
    );
    let TranscriptContent::Subagent {
        status: TranscriptSubagentStatus::Finished { result, status },
        ..
    } = decode(&page.rows[0]).expect("child result")
    else {
        panic!("missing finished child body");
    };
    assert_eq!(status, rw_types::SubagentStatus::Completed);
    assert!(!result.complete);
    assert_eq!(result.source.sequence, SequenceId(1));
    let document = TranscriptDocument::from_event(event, &result.source, 65536)
        .expect("authoritative full result");
    assert_eq!(
        document.chunk(0, 65536).expect("complete result").text,
        "complete output ".repeat(1000)
    );
}
