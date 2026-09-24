use super::fixtures::support::{config, protocol_meta};
use crate::engine::{AgentLoopError, ModelDriver, builtin_hook_dispatcher};
use async_trait::async_trait;
use rw_providers::{BoxEventStream, ProviderRequest};
use rw_tools::ToolRegistry;
use rw_types::{ClientCommand, ClientRole, CommandOutcome, SessionId, config::PermissionDecision};
use std::sync::Arc;

struct MissingModel {
    lazy: bool,
}
#[async_trait]
impl ModelDriver for MissingModel {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
    fn has_model_alias(&self, _: &str) -> bool {
        self.lazy
    }
    fn needs_initial_preparation(&self) -> bool {
        self.lazy
    }
    async fn prepare_model(&self, _: &str) -> Result<(), AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "missing provider configuration".into(),
        ))
    }
    fn stream(
        &self,
        _: &str,
        _: ProviderRequest,
        _: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        panic!("an unconfigured model must never receive a turn")
    }
}

#[tokio::test]
async fn missing_model_rejects_prompt_without_history_and_keeps_help_available() {
    for lazy in [false, true] {
        let root = tempfile::tempdir().expect("workspace");
        let handle = super::fixtures::history::spawn(config(
            root.path(),
            Arc::new(MissingModel { lazy }),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .await
        .expect("actor");
        let session_id = SessionId("fixture-session".into());
        let attached = handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach");
        assert_eq!(attached, CommandOutcome::Accepted {});
        let before = handle.dump_prompt(None).await.expect("initial history");
        let outcome = handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "prompt"),
                session_id: session_id.clone(),
                content: "hello".into(),
                attachments: vec![],
            })
            .await
            .expect("prompt response");
        assert!(matches!(outcome, CommandOutcome::Rejected { error }
        if error.code == "no_model_selected" && error.message.contains("/model ")));
        let after = handle.dump_prompt(None).await.expect("unchanged history");
        assert_eq!(after.turns, before.turns);
        let help = handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "help"),
                session_id,
                content: "/help".into(),
                attachments: vec![],
            })
            .await
            .expect("help response");
        assert_eq!(help, CommandOutcome::Accepted {});
        handle.close().await.expect("close");
    }
}
