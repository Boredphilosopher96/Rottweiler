#![cfg(test)]

use crate::PermissionGate;
use crate::engine::AgentLoopError;
use crate::engine::commands::NoopFolderTrustController;
use crate::engine::commands::NoopWorkspaceRootController;
use crate::engine::commands::builtin_command_registry;
use crate::engine::durability::NoopSessionEventSink;
use crate::engine::durability::SessionEventSink;
use crate::engine::event_clock::EventClock;
use crate::engine::model::ModelDriver;
use crate::engine::mutation_checkpoints::NoopMutationCheckpointCoordinator;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::SessionRecoveredState;
use crate::engine::projection::recovered_pending_event;
use crate::engine::redaction::NoopSecretRedactor;
use crate::engine::redaction::SecretRedactor;
use crate::engine::replay::SessionReplayLimits;
use crate::engine::session::SessionActorConfig;
use crate::engine::session::SessionHandle;
use crate::engine::session::SessionSubscription;
use crate::engine::session_extension::NoopSessionExtensionController;
use crate::engine::session_mode_name;
use crate::engine::tests::fixtures::models::ProviderScript;
use crate::engine::unavailable_cost;
use async_trait::async_trait;
use rw_ext::HookDispatcher;
use rw_ext::ModeRegistry;
use rw_providers::FinishReason;
use rw_providers::ProviderEvent;
use rw_providers::TokenUsage;
use rw_tools::CapabilityManifest;
use rw_tools::ToolDescriptor;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::ClientId;
use rw_types::CommandMeta;
use rw_types::EngineEvent;
use rw_types::EventMeta;
use rw_types::ModeId;
use rw_types::PROTOCOL_VERSION;
use rw_types::PermissionStateDescriptor;
use rw_types::RequestId;
use rw_types::Role;
use rw_types::SequenceId;
use rw_types::SessionId;
use rw_types::SessionMode;
use rw_types::SubagentId;
use rw_types::ToolCapability;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::Usage;
use rw_types::config::PermissionDecision;
use rw_types::config::ThinkingLevel;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

#[async_trait]
pub(in crate::engine::tests) trait TestEventSinkExt:
    SessionEventSink
{
    async fn test_events_after(
        &self,
        mut after: Option<SequenceId>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        let view = self.capture_read_view()?;
        let mut events = Vec::new();
        while after != view.last_sequence() {
            let page = view
                .read_page(after, SessionReplayLimits::default())
                .await?;
            if page.is_empty() {
                return Err(AgentLoopError::Persistence(
                    "fixture view did not advance".to_owned(),
                ));
            }
            after = page
                .last()
                .and_then(EngineEvent::meta)
                .map(|meta| meta.sequence_id);
            events.extend(page);
        }
        Ok(events)
    }
}

impl<T: SessionEventSink + ?Sized> TestEventSinkExt for T {}

pub(in crate::engine::tests) fn has_command(handle: &SessionHandle, name: &str) -> bool {
    handle
        .command_descriptors()
        .iter()
        .any(|descriptor| descriptor.name() == name)
}

pub(in crate::engine::tests) fn fixture_subagent_result(id: &str) -> rw_types::SubagentResult {
    rw_types::SubagentResult {
        subagent_id: SubagentId(id.to_owned()),
        session_id: SessionId(format!("child-{id}")),
        status: rw_types::SubagentStatus::Completed,
        final_text: "done".to_owned(),
        touched_files: Vec::new(),
        diff_artifact: None,
        usage: Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: unavailable_cost(),
        turns: 1,
        duration_millis: 0,
    }
}

pub(in crate::engine::tests) fn canonical_json_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

pub(in crate::engine::tests) fn workspace_tree_bytes(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<String, Vec<u8>>) {
        let mut entries = std::fs::read_dir(path)
            .expect("read workspace tree")
            .collect::<Result<Vec<_>, _>>()
            .expect("workspace entries");
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let relative = entry
                .path()
                .strip_prefix(root)
                .expect("workspace relative path")
                .to_path_buf();
            if relative
                .components()
                .next()
                .is_some_and(|component| component.as_os_str() == std::ffi::OsStr::new(".git"))
            {
                continue;
            }
            let key = relative.to_string_lossy().into_owned();
            let kind = entry.file_type().expect("workspace entry type");
            if kind.is_dir() {
                snapshot.insert(format!("{key}/"), Vec::new());
                visit(root, &entry.path(), snapshot);
            } else if kind.is_symlink() {
                snapshot.insert(
                    key,
                    std::fs::read_link(entry.path())
                        .expect("symlink target")
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                );
            } else {
                snapshot.insert(key, std::fs::read(entry.path()).expect("workspace file"));
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

pub(in crate::engine::tests) fn descriptor(name: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: format!("fixture {name}"),
        input_schema: json!({"type": "object"}),
        capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
    }
}

pub(in crate::engine::tests) fn tool_script(
    calls: &[(&str, &str, Value)],
    usage: &[TokenUsage],
) -> ProviderScript {
    let mut events = vec![Ok(ProviderEvent::MessageStart {
        model: "fixture-model".to_owned(),
    })];
    for (id, name, arguments) in calls {
        events.push(Ok(ProviderEvent::ToolCallStart {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
        }));
        events.push(Ok(ProviderEvent::ToolCallEnd {
            id: (*id).to_owned(),
            arguments: arguments.clone(),
        }));
    }
    events.extend(
        usage
            .iter()
            .copied()
            .map(|usage| Ok(ProviderEvent::Usage { usage })),
    );
    events.push(Ok(ProviderEvent::Finished {
        reason: FinishReason::ToolCalls,
    }));
    events
}

pub(in crate::engine::tests) fn stop_script(text: &str, usage: &[TokenUsage]) -> ProviderScript {
    let mut events = vec![
        Ok(ProviderEvent::MessageStart {
            model: "fixture-model".to_owned(),
        }),
        Ok(ProviderEvent::TextDelta {
            text: text.to_owned(),
        }),
    ];
    events.extend(
        usage
            .iter()
            .copied()
            .map(|usage| Ok(ProviderEvent::Usage { usage })),
    );
    events.push(Ok(ProviderEvent::Finished {
        reason: FinishReason::Stop,
    }));
    events
}

pub(in crate::engine::tests) fn config(
    root: &Path,
    model: Arc<dyn ModelDriver>,
    tools: Arc<ToolRegistry>,
    permissions: PermissionDecision,
    hooks: HookDispatcher,
) -> SessionActorConfig {
    SessionActorConfig {
        budget_session_id: SessionId("fixture-session".to_owned()),
        session_id: SessionId("fixture-session".to_owned()),
        workspace_root: root.to_path_buf(),
        additional_workspace_roots: Vec::new(),
        workspace_generation: 0,
        initial_session_context: Vec::new(),
        startup_notifications: Vec::new(),
        model_alias: "fast".to_owned(),
        model,
        tools,
        permissions: Arc::new(PermissionGate::new(permissions)),
        hooks: Arc::new(hooks),
        commands: Arc::new(builtin_command_registry().expect("built-in commands")),
        modes: Arc::new(ModeRegistry::builtins().expect("built-in modes")),
        event_sink: Arc::new(NoopSessionEventSink::default()),
        event_clock: Arc::new(FixedClock),
        provider_admission: crate::provider_admission::testing::admission(),
        secret_redactor: Arc::new(NoopSecretRedactor),
        checkpoints: Arc::new(NoopMutationCheckpointCoordinator),
        folder_trust: Arc::new(NoopFolderTrustController),
        workspace_roots: Arc::new(NoopWorkspaceRootController),
        extension_development: Arc::new(NoopSessionExtensionController),
        resources: Arc::new(crate::NoopSessionResources),
        recovered: SessionRecoveredState::default(),
        max_turns: 10,
        identical_tool_failure_limit: 5,
        max_output_tokens: 256,
        thinking: ThinkingLevel::Off,
        event_capacity: 256,
    }
}

#[derive(Debug)]
pub(in crate::engine::tests) struct FixedClock;

impl EventClock for FixedClock {
    fn emitted_at(&self) -> String {
        "2026-01-02T03:04:05.006Z".to_owned()
    }
}

#[derive(Debug)]
pub(in crate::engine::tests) struct ShellSecretRedactor;

impl SecretRedactor for ShellSecretRedactor {
    fn redact(&self, text: &str) -> String {
        if text.starts_with("COLLAPSE:") {
            return "useful [REDACTED] output".to_owned();
        }
        text.replace("SHELL_SECRET", "[REDACTED]")
    }
}

#[derive(Debug)]
pub(in crate::engine::tests) struct CanarySecretRedactor;

impl SecretRedactor for CanarySecretRedactor {
    fn redact(&self, text: &str) -> String {
        text.replace("KNOWN_CANARY", "[REDACTED]")
    }

    fn max_secret_bytes(&self) -> usize {
        "KNOWN_CANARY".len()
    }
}

#[derive(Debug)]
pub(in crate::engine::tests) struct PemSecretRedactor;

impl SecretRedactor for PemSecretRedactor {
    fn redact(&self, text: &str) -> String {
        let Some(start) = text.find("-----BEGIN PRIVATE KEY-----") else {
            return text.to_owned();
        };
        let Some(relative_end) = text[start..].find("-----END PRIVATE KEY-----") else {
            return text.to_owned();
        };
        let end = start + relative_end + "-----END PRIVATE KEY-----".len();
        format!("{}[REDACTED]{}", &text[..start], &text[end..])
    }

    fn max_secret_bytes(&self) -> usize {
        64
    }

    fn has_incomplete_secret_envelope(&self, text: &str) -> bool {
        text.rfind("-----BEGIN PRIVATE KEY-----")
            .is_some_and(|start| !text[start..].contains("-----END PRIVATE KEY-----"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::engine::tests) struct SessionEvent {
    pub(in crate::engine::tests) version: u16,
    pub(in crate::engine::tests) sequence: SequenceId,
    pub(in crate::engine::tests) kind: PendingEvent,
    pub(in crate::engine::tests) wire: EngineEvent,
}

pub(in crate::engine::tests) fn observe_event(wire: EngineEvent) -> Option<SessionEvent> {
    let meta = wire.meta()?.clone();
    let kind = recovered_pending_event(&wire).ok()??;
    Some(SessionEvent {
        version: meta.protocol_version,
        sequence: meta.sequence_id,
        kind,
        wire,
    })
}

pub(in crate::engine::tests) fn wire_event(sequence: u64, kind: PendingEvent) -> EngineEvent {
    kind.stamp(EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("fixture-session".to_owned()),
        sequence_id: SequenceId(sequence),
        emitted_at: FixedClock.emitted_at(),
        caused_by: None,
    })
}

pub(in crate::engine::tests) fn protocol_meta(client: &str, request: &str) -> CommandMeta {
    CommandMeta {
        protocol_version: PROTOCOL_VERSION,
        client_id: ClientId(client.to_owned()),
        request_id: RequestId(request.to_owned()),
    }
}

pub(in crate::engine::tests) fn wire_mode(mode: SessionMode) -> ModeId {
    ModeId(session_mode_name(mode).to_owned())
}

pub(in crate::engine::tests) async fn next_matching(
    receiver: &mut SessionSubscription,
    mut matches: impl FnMut(&PendingEvent) -> bool,
) -> SessionEvent {
    loop {
        let wire = timeout(Duration::from_secs(3), receiver.recv())
            .await
            .expect("event timeout")
            .expect("event channel");
        let Some(event) = observe_event(wire) else {
            continue;
        };
        if matches(&event.kind) {
            return event;
        }
    }
}

pub(in crate::engine::tests) async fn next_permission_state(
    receiver: &mut SessionSubscription,
) -> PermissionStateDescriptor {
    loop {
        let event = timeout(Duration::from_secs(3), receiver.recv())
            .await
            .expect("permission event timeout")
            .expect("permission event channel");
        if let EngineEvent::PermissionsListed { permissions, .. } = event {
            return permissions;
        }
    }
}

pub(in crate::engine::tests) async fn collect_turn(
    receiver: &mut SessionSubscription,
) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    loop {
        let wire = timeout(Duration::from_secs(3), receiver.recv())
            .await
            .expect("event timeout")
            .expect("event channel");
        let Some(event) = observe_event(wire) else {
            continue;
        };
        let done = matches!(event.kind, PendingEvent::TurnFinished { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

pub(in crate::engine::tests) async fn collect_wire_turn(
    receiver: &mut SessionSubscription,
) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    loop {
        let routed = timeout(Duration::from_secs(3), receiver.receiver.recv())
            .await
            .expect("wire event timeout")
            .expect("wire event channel");
        if routed
            .target
            .as_ref()
            .is_some_and(|target| target != &receiver.client_id)
        {
            continue;
        }
        let event = routed.event;
        let done = matches!(event, EngineEvent::TurnFinished { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

pub(in crate::engine::tests) fn text_turn(role: Role, text: impl Into<String>) -> Turn {
    Turn {
        role,
        blocks: vec![Block::Text { text: text.into() }],
        meta: TurnMeta::default(),
    }
}
