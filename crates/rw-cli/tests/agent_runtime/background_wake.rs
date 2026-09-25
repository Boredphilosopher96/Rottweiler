//! A one-shot run stays open until background children have reported.
use super::*;
use rw_types::conversation_input::ContextSelection;

fn background_spawn_script(path: &Path) {
    write_script(
        path,
        vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "spawn-child".to_owned(),
                    name: "spawn_agent".to_owned(),
                },
                ProviderEvent::ToolCallEnd {
                    id: "spawn-child".to_owned(),
                    arguments: json!({
                        "action": "spawn",
                        "task": "inspect in the background",
                        "agent": "explore",
                        "isolation": "shared",
                    }),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            // The parent's closing reply and the child's report run concurrently;
            // identical entries keep the outcome independent of which is first.
            text_events("step finished"),
            text_events("step finished"),
            text_events("woke with the child report"),
        ],
    );
}

fn print_run(format: &str) -> (std::process::Output, TempDir) {
    let root = tempdir().expect("root");
    let run = TestRun::new(&root, &format!("background-wake-{format}"));
    let script = root.path().join("background-wake.json");
    background_spawn_script(&script);
    let output = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "start a background child and finish",
            "--permission-mode",
            "yolo",
            "--output-format",
            format,
            "--in-memory-replay-script",
            script.to_str().expect("script"),
            // Each event waits, so the child reports after the parent's turn ends.
            "--record-script-delay-ms",
            "150",
        ])
        .output()
        .expect("rw binary");
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    (output, root)
}

#[test]
fn print_mode_waits_for_a_background_child_and_prints_the_turn_it_wakes() {
    let (output, _root) = print_run("stream-json");
    let events = parse_stream(&output.stdout);
    let finished = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::SubagentFinished { meta, result, .. } => {
                Some((meta.sequence_id, result.status.clone()))
            }
            _ => None,
        })
        .expect("the child finished before the run ended");
    assert_eq!(
        finished.1,
        rw_types::SubagentStatus::Completed,
        "closing the run must not cancel the child; events: {events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::ConversationContextCommitted {
                selection: ContextSelection::ChildResult { source },
                ..
            } if *source == finished.0
        )),
        "the child's report reached the model; events: {events:#?}"
    );
    let turns = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::TurnFinished { status, .. } => Some(status.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(turns, [TurnStatus::Completed, TurnStatus::Completed]);
    let woke = events
        .iter()
        .position(|event| {
            matches!(event, EngineEvent::TextDelta { text, .. } if text == "woke with the child report")
        })
        .expect("the woken turn's output is part of the run");
    let child_done = events
        .iter()
        .position(|event| matches!(event, EngineEvent::SubagentFinished { .. }))
        .expect("child finished");
    assert!(child_done < woke);
}

#[test]
fn json_print_aggregates_the_woken_turn() {
    let (output, _root) = print_run("json");
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_eq!(result["status"], "completed");
    let text = result["text"].as_str().expect("text");
    assert!(
        text.ends_with("woke with the child report"),
        "aggregate text: {text:?}"
    );
}
