use super::{
    CanonicalRecovery, MAX_COMPLETION_NOTICES,
    tests::{append, catch_up, terminal},
};
use crate::{SubagentHandle, engine::PendingEvent};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{ModelContextTransfer, SequenceId, SessionId, SubagentId};

fn spawn(id: &str) -> PendingEvent {
    PendingEvent::SubagentSpawned {
        subagent_id: SubagentId(id.into()),
        child_session_id: SessionId(format!("session-{id}")),
        task: "work".into(),
    }
}
fn finish(id: &str) -> PendingEvent {
    let handle = SubagentHandle {
        subagent_id: SubagentId(id.into()),
        session_id: SessionId(format!("session-{id}")),
    };
    let mut result = crate::interrupted_subagent_recovery_result(&handle);
    result.final_text = "🙂result".repeat(200);
    if id == "early" {
        result.diff_artifact = Some(rw_types::DiffArtifact {
            id: "large-artifact".into(),
            base_commit: "a".repeat(40),
            touched_files: vec![],
            unified_diff: "x".repeat(1024 * 1024),
        });
    }
    PendingEvent::SubagentFinished {
        subagent_id: handle.subagent_id,
        result,
    }
}

#[test]
fn completion_context_is_bounded_replayable_and_historical_selection_is_exact()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let modes = ModeRegistry::builtins()?;
    let mut journal = SegmentedJournal::open(root.path(), "canonical")?;
    let mut index = CanonicalRecovery::open(&journal.read_view(), &modes, None)?;
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            spawn("early"),
            finish("early"),
            terminal(1),
        ],
    );
    catch_up(&mut index, &journal.read_view(), &modes);
    let captured = index.snapshot()?.bind_source(&journal.read_view())?;
    let notices = captured.completion_notices()?;
    assert_eq!(notices.len(), 1);
    assert!(notices[0].text.len() < 2048);
    assert!(notices[0].text.contains("untrusted result excerpt"));
    let selected = notices[0].sequence;
    // A later child finishes after request assembly but before usage is persisted.
    let mut usage = super::prompt_tests::usage(2);
    if let PendingEvent::ContextUsage {
        completion_sources, ..
    } = &mut usage
    {
        completion_sources.push(selected);
    }
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 2 },
            spawn("late"),
            finish("late"),
            usage,
            terminal(2),
        ],
    );
    catch_up(&mut index, &journal.read_view(), &modes);
    let history = index.snapshot()?.bind_source(&journal.read_view())?;
    assert_eq!(history.completion_notices()?.len(), 2);
    assert_eq!(
        notice_sequences(&history.prompt_at_turn(2)?.completion_notices()?),
        vec![selected]
    );
    assert_eq!(captured.completion_notices()?.len(), 1);
    append(
        &mut journal,
        vec![
            PendingEvent::CompactionStarted {
                reason: rw_types::CompactionReason::Manual,
            },
            PendingEvent::CompactionFinished {
                summary_turn: 2,
                reclaimed_tokens: 0,
                usage: None,
                cost: None,
            },
        ],
    );
    catch_up(&mut index, &journal.read_view(), &modes);
    assert_eq!(
        index
            .snapshot()?
            .bind_source(&journal.read_view())?
            .completion_notices()?
            .len(),
        2
    );
    // Repeated reads never append another notice. Retention evicts oldest sources.
    for n in 0..12 {
        append(
            &mut journal,
            vec![spawn(&format!("next-{n}")), finish(&format!("next-{n}"))],
        );
    }
    catch_up(&mut index, &journal.read_view(), &modes);
    let before = index
        .snapshot()?
        .bind_source(&journal.read_view())?
        .completion_notices()?;
    assert_eq!(before.len(), MAX_COMPLETION_NOTICES);
    assert!(before.iter().all(|n| n.sequence > selected));
    drop(history);
    drop(captured);
    drop(index);
    let mut index = CanonicalRecovery::open(&journal.read_view(), &modes, None)?;
    catch_up(&mut index, &journal.read_view(), &modes);
    let reopened = index.snapshot()?.bind_source(&journal.read_view())?;
    assert_eq!(
        notice_sequences(&before),
        notice_sequences(&reopened.completion_notices()?)
    );
    assert_eq!(
        reopened.prompt_at_turn(2)?.completion_notices()?[0].sequence,
        selected
    );
    Ok(())
}

#[test]
fn rewind_discards_late_results_of_discarded_spawns_and_clear_removes_notices()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let modes = ModeRegistry::builtins()?;
    let mut journal = SegmentedJournal::open(root.path(), "canonical")?;
    let mut index = CanonicalRecovery::open(&journal.read_view(), &modes, None)?;
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            spawn("keep"),
            terminal(1),
            PendingEvent::TurnStarted { turn: 2 },
            spawn("discard"),
            terminal(2),
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
            finish("discard"),
            finish("keep"),
        ],
    );
    catch_up(&mut index, &journal.read_view(), &modes);
    let notices = index
        .snapshot()?
        .bind_source(&journal.read_view())?
        .completion_notices()?;
    assert_eq!(notices.len(), 1);
    assert!(notices[0].text.contains("Child agent keep"));
    append(
        &mut journal,
        vec![PendingEvent::ModelContextCleared {
            strategy: ModelContextTransfer::StartWithoutContext,
        }],
    );
    catch_up(&mut index, &journal.read_view(), &modes);
    assert!(
        index
            .snapshot()?
            .bind_source(&journal.read_view())?
            .completion_notices()?
            .is_empty()
    );
    Ok(())
}

#[test]
fn duplicate_finish_and_selector_corruption_cannot_grow_notice_state()
-> Result<(), Box<dyn std::error::Error>> {
    let mut sources = super::completions::CompletionSources::default();
    sources.spawned("child".into(), 1)?;
    sources.finished("child", SequenceId(2));
    sources.finished("child", SequenceId(3));
    assert_eq!(sources.retained.len(), 1);
    assert!(sources.validate(2).is_err());
    sources.validate(3)?;
    Ok(())
}

fn notice_sequences(notices: &[super::CompletionNotice]) -> Vec<SequenceId> {
    notices.iter().map(|notice| notice.sequence).collect()
}
