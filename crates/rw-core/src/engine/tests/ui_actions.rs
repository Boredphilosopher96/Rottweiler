//! UI actions preserve driver ownership and execute approved command arguments.
use super::fixtures::{
    controllers::EchoCommand,
    models::ScriptedModel,
    support::{config, next_matching, protocol_meta},
};
use crate::engine::{
    AgentLoopError, builtin_hook_dispatcher, pending_event::PendingEvent, session::SessionActor,
};
use crate::ui::{BoundUiCommand, UiRegistry};
use rw_ext::{CommandDescriptor, CommandRegistry};
use rw_tools::ToolRegistry;
use rw_types::extension_ui::{
    UiActionRequest, UiActionTarget, UiCatalog, UiContributionOwner, UiGenerationId, UiPanels,
    UiPresentation,
};
use rw_types::{ClientCommand, ClientRole, CommandOutcome, SessionId, config::PermissionDecision};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Registry {
    owner: UiContributionOwner,
    resolved: Arc<AtomicUsize>,
}
impl UiRegistry for Registry {
    fn owns(&self, owner: &UiContributionOwner) -> bool {
        owner == &self.owner
    }
    fn catalog(&self) -> Result<UiCatalog, AgentLoopError> {
        Ok(UiCatalog {
            entries: Vec::new(),
        })
    }
    fn panels(&self) -> Result<UiPanels, AgentLoopError> {
        Ok(UiPanels { panels: Vec::new() })
    }
    fn resolve_action(
        &self,
        request: &UiActionRequest,
        source: Option<&UiPresentation>,
    ) -> Result<BoundUiCommand, AgentLoopError> {
        assert_eq!(request.owner, self.owner);
        assert!(source.is_none());
        self.resolved.fetch_add(1, Ordering::SeqCst);
        let mut commands = CommandRegistry::new();
        commands
            .register(
                CommandDescriptor::new("echo", "approved action"),
                EchoCommand,
            )
            .expect("register");
        commands
            .bind("echo", "approved host arguments".into())
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))
    }
}

#[tokio::test]
async fn driver_ui_action_uses_bound_arguments_and_observer_never_resolves() {
    let root = tempfile::TempDir::new().expect("root");
    let owner = UiContributionOwner {
        extension: "fixture".into(),
        generation: UiGenerationId::from_bytes([1; 16]),
    };
    let resolved = Arc::new(AtomicUsize::new(0));
    let mut configuration = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    configuration.ui = Arc::new(Registry {
        owner: owner.clone(),
        resolved: resolved.clone(),
    });
    let handle = crate::engine::tests::fixtures::history::spawn(configuration)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("events");
    let session = SessionId("fixture-session".into());
    let request = UiActionRequest {
        owner,
        contribution_id: "panel".into(),
        action_id: "run".into(),
        target: UiActionTarget::Panel { revision: 1 },
    };
    handle
        .dispatch(ClientCommand::AttachSession {
            meta: protocol_meta("driver", "attach"),
            session_id: session.clone(),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        })
        .await
        .expect("attach");
    assert!(matches!(
        handle
            .dispatch(ClientCommand::InvokeUiAction {
                meta: protocol_meta("observer", "deny"),
                session_id: session.clone(),
                request: request.clone()
            })
            .await
            .expect("deny"),
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(resolved.load(Ordering::SeqCst), 0);
    assert_eq!(
        handle
            .dispatch(ClientCommand::InvokeUiAction {
                meta: protocol_meta("driver", "invoke"),
                session_id: session,
                request
            })
            .await
            .expect("action"),
        CommandOutcome::Accepted {}
    );
    let finished = next_matching(
        &mut events,
        |event| matches!(event, PendingEvent::CommandFinished { name, .. } if name == "echo"),
    )
    .await;
    assert!(
        matches!(finished.kind, PendingEvent::CommandFinished { message, .. } if message == "approved host arguments")
    );
    assert_eq!(resolved.load(Ordering::SeqCst), 1);
    assert!(
        handle
            .ui_catalog()
            .await
            .expect("catalog")
            .entries
            .is_empty()
    );
    assert!(handle.ui_panels().await.expect("panels").panels.is_empty());
    handle.close().await.expect("settled close");
}
