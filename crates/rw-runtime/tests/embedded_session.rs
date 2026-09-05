//! Exercise the embedding API in an isolated process with no CLI or terminal.
#![allow(clippy::expect_used)]
use rw_core::{EngineEvent, TurnStatus};
use rw_providers::{FinishReason, ProviderEvent};
use rw_runtime::session::{LocalSessionOptions, LocalSessionPurpose, compose_local_session};
use rw_types::PermissionModeDescriptor;
use serde_json::json;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const RESPONSE: &str = "captured embedding response";

fn main() -> Result<()> {
    if std::env::var_os("RW_EMBEDDED_FIXTURE").is_some() {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(exercise());
    }
    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    fs::create_dir(&home)?;
    fs::create_dir(&workspace)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
    }
    let mut child = Command::new(std::env::current_exe()?)
        .env_clear()
        .env("HOME", &home)
        .env("ROTTWEILER_HOME", &home)
        .env("RW_EMBEDDED_FIXTURE", "1")
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_mins(1);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill()?;
            let output = child.wait_with_output()?;
            return Err(format!(
                "embedded session timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "runtime wrote to embedder stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "runtime wrote to embedder stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn options(script: &Path, resume: Option<String>) -> LocalSessionOptions {
    LocalSessionOptions {
        permission_mode: Some(PermissionModeDescriptor::AutoSafe),
        max_turns: 4,
        resume,
        continue_latest: false,
        replay_dir: None,
        record_replay_script: None,
        in_memory_replay_script: Some(script.to_owned()),
        record_script_delay_ms: 0,
        activate_fixture_extensions: false,
        replay_provider: "embedded-fixture".to_owned(),
        model: None,
        additional_workspaces: Vec::new(),
        dangerously_trust: false,
        purpose: LocalSessionPurpose::Conversation { interactive: false },
    }
}

async fn exercise() -> Result<()> {
    let script = std::env::current_dir()?.join("provider.json");
    let question = vec![
        ProviderEvent::ToolCallStart {
            id: "question".to_owned(),
            name: "ask_user".to_owned(),
        },
        ProviderEvent::ToolCallEnd {
            id: "question".to_owned(),
            arguments: json!({"question":"Pick one", "options":["first","second"]}),
        },
        ProviderEvent::Finished {
            reason: FinishReason::ToolCalls,
        },
    ];
    fs::write(
        &script,
        serde_json::to_vec(&vec![
            question.clone(),
            vec![
                ProviderEvent::TextDelta {
                    text: RESPONSE.to_owned(),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ])?,
    )?;
    let session = compose_local_session(options(&script, None)).await?;
    let session_id = session.session_id().to_owned();
    let mut events = session.handle().subscribe()?;
    events.prime().await?;
    session.handle().send_message("ask me".to_owned()).await?;
    let mut text = String::new();
    let mut answered = false;
    loop {
        match events.recv().await? {
            EngineEvent::QuestionAsked { question_id, .. } => {
                // The embedder, not composition, chooses the answer.
                answered = true;
                session
                    .handle()
                    .answer_question(question_id, vec!["second".to_owned()])
                    .await?;
            }
            EngineEvent::TextDelta { text: delta, .. } => text.push_str(&delta),
            EngineEvent::TurnFinished { status, .. } => {
                assert_eq!(status, TurnStatus::Completed);
                break;
            }
            _ => {}
        }
    }
    assert!(answered);
    assert_eq!(text, RESPONSE);
    session.close().await?;
    drop(events);
    drop(session);

    fs::write(&script, serde_json::to_vec(&vec![question])?)?;
    let resumed = compose_local_session(options(&script, Some(session_id.clone()))).await?;
    assert_eq!(resumed.session_id(), session_id);
    let mut events = resumed.handle().subscribe()?;
    events.prime().await?;
    resumed
        .handle()
        .send_message("ask again".to_owned())
        .await?;
    let mut interrupted = false;
    let mut accepted = false;
    loop {
        match events.recv().await? {
            EngineEvent::UserMessageAccepted { content, .. } if content == "ask again" => {
                accepted = true;
            }
            EngineEvent::QuestionAsked { .. } if accepted => {
                assert!(resumed.handle().interrupt().await?);
                interrupted = true;
            }
            EngineEvent::TurnFinished { status, .. } if interrupted => {
                assert_eq!(status, TurnStatus::Interrupted);
                break;
            }
            _ => {}
        }
    }
    resumed.close().await?;
    Ok(())
}
