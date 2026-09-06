use super::SessionActorRecovery;
use crate::PermissionRequest;
use crate::engine::AgentLoopError;
use crate::engine::MessageDisposition;
use crate::engine::SessionSnapshot;
use crate::engine::event_clock::EventClock;
use crate::engine::mode_permission_base;
use crate::engine::mutation_checkpoints::RewindCheckpoint;
use crate::engine::projection::RecoveredUserShell;
use crate::engine::session_mode_name;
use crate::engine::task_ownership;
use crate::engine::turn::RunningTurn;
use rw_context::Budgeter;
use rw_ext::ModeRegistry;
use rw_types::ApprovalBinding;
use rw_types::ApprovalDecision;
use rw_types::Attachment;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandMeta;
use rw_types::CommandOutcome;
use rw_types::ContextSnapshot;
use rw_types::CostSnapshot;
use rw_types::ModeId;
use rw_types::ModelAlias;
use rw_types::ModelContextTransfer;
use rw_types::PlanArtifact;
use rw_types::PromptDump;
use rw_types::RequestId;
use rw_types::SessionId;
use rw_types::SessionMode;
use rw_types::ShellId;
use rw_types::SubagentId;
use rw_types::Turn;
use rw_types::UnrestorablePath;
use rw_types::config::ThinkingLevel;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::oneshot;

pub(in crate::engine) enum ActorCommand {
    LiveState {
        respond:
            oneshot::Sender<Result<rw_types::session_state::SessionStateSnapshot, AgentLoopError>>,
    },
    ChildControls {
        respond: oneshot::Sender<
            Result<rw_types::family_controls::ChildControlsSnapshot, AgentLoopError>,
        >,
    },
    ChildControl {
        authority: super::FamilyControlAuthority,
        command: ClientCommand,
        expected_revision: rw_types::SequenceId,
        respond: oneshot::Sender<CommandOutcome>,
        completion: oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>,
    },
    Controls {
        respond: oneshot::Sender<
            Result<rw_types::session_controls::SessionControlsSnapshot, AgentLoopError>,
        >,
    },
    UiCatalog {
        respond: oneshot::Sender<Result<rw_types::extension_ui::UiCatalog, AgentLoopError>>,
    },
    UiPanels {
        respond: oneshot::Sender<Result<rw_types::extension_ui::UiPanels, AgentLoopError>>,
    },
    Protocol {
        command: ClientCommand,
        respond: oneshot::Sender<CommandOutcome>,
        completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
    },
    CompleteUserShell {
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    RecordSubagentSpawned {
        subagent_id: SubagentId,
        child_session_id: SessionId,
        task: String,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    RecordSubagentFinished {
        result: rw_types::SubagentResult,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    PublishSubagentProgress(Arc<crate::engine::turn::child_progress::ChildProgressSlot>),
    PluginInjectMessage {
        plugin_id: String,
        content: String,
        respond: oneshot::Sender<Result<MessageDisposition, AgentLoopError>>,
    },
    PluginContextRead {
        request: rw_types::extension_control::ExtensionContextRead,
        respond: oneshot::Sender<
            Result<rw_types::extension_control::ExtensionContextPage, AgentLoopError>,
        >,
    },
    PluginToolCall {
        request: rw_types::extension_tools::ExtensionToolCall,
        respond: oneshot::Sender<
            Result<rw_types::extension_tools::ExtensionToolOutcome, AgentLoopError>,
        >,
    },
    PluginControl {
        origin: Option<rw_types::extension_invocation::ExtensionInvocationId>,
        control: rw_types::extension_control::ExtensionControl,
        respond: oneshot::Sender<
            Result<rw_types::extension_control::ExtensionControlOutcome, AgentLoopError>,
        >,
    },
    PluginQuery {
        respond: oneshot::Sender<
            Result<rw_types::extension_contract::ExtensionSessionSnapshot, AgentLoopError>,
        >,
    },
    PluginStateRead {
        plugin_id: String,
        respond: oneshot::Sender<
            Result<rw_types::extension_contract::ExtensionStateSnapshot, AgentLoopError>,
        >,
    },
    PluginStateCommit {
        plugin_id: String,
        transaction: rw_types::extension_contract::ExtensionStateTransaction,
        respond: oneshot::Sender<
            Result<rw_types::extension_contract::ExtensionStateCommitOutcome, AgentLoopError>,
        >,
    },
    PluginSetStatus {
        plugin_id: String,
        status: String,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    PluginNotify {
        plugin_id: String,
        title: String,
        message: String,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    SendMessage {
        command_meta: CommandMeta,
        content: String,
        attachments: Vec<Attachment>,
        observed_turn: u64,
        respond: oneshot::Sender<Result<MessageDisposition, AgentLoopError>>,
    },
    #[cfg(test)]
    Interrupt {
        target_turn: u64,
        respond: oneshot::Sender<bool>,
    },
    Snapshot {
        respond: oneshot::Sender<SessionSnapshot>,
    },
}

pub(in crate::engine) enum ProtocolCompletion {
    Message(MessageDisposition),
    Rewind(Vec<UnrestorablePath>),
    Context(crate::recovery::HistoryRead<ContextSnapshot>),
    Cost(Box<CostSnapshot>),
    Prompt(crate::recovery::HistoryRead<PromptDump>),
    Unit,
}

#[allow(clippy::struct_excessive_bools)]
pub(in crate::engine) struct ActorState {
    pub(in crate::engine) pending_context_read:
        Option<crate::engine::dispatch::context_job::PendingRead>,
    pub(in crate::engine) live: super::live_state::LiveState,
    pub(in crate::engine) pending_plugin_tool:
        Option<crate::engine::turn::plugin_tool::PendingPluginTool>,
    pub(in crate::engine) pending_model_preparation:
        Option<crate::engine::dispatch::model_job::PendingPreparation>,
    pub(in crate::engine) pending_command:
        Option<crate::engine::dispatch::command_job::PendingCommand>,
    pub(in crate::engine) session_id: SessionId,
    _recovery_source: Option<crate::engine::recovery::HistoryRead<()>>,
    pub(in crate::engine) session_title: Option<String>,
    pub(in crate::engine) title_generation_started: bool,
    pub(in crate::engine) event_clock: Arc<dyn EventClock>,
    pub(in crate::engine) conversation_turns: u64,
    system_turns: u64,
    system_resolved_model: Option<String>,
    pub(in crate::engine) title_prompt: Option<String>,
    pub(in crate::engine) has_assistant_text: bool,
    pub(in crate::engine) approved_plan_item: Option<rw_types::ContextItemId>,
    pub(in crate::engine) resolved_model: Option<String>,
    // The bootstrap allowance owns these pending bodies while a selected child
    // exposes controls without implicitly resuming inference.
    pub(in crate::engine) suspended_inputs: Option<Vec<crate::recovery::RecoveredMessage>>,
    pub(in crate::engine) queued: VecDeque<String>,
    pub(in crate::engine) queued_positions: VecDeque<u64>,
    pub(in crate::engine) running: Option<RunningTurn>,
    pub(in crate::engine) pending_approvals: BTreeMap<String, PendingApproval>,
    pub(in crate::engine) next_turn: u64,
    pub(in crate::engine) completed_turns: u64,
    pub(in crate::engine) sequence: Option<u64>,
    pub(in crate::engine) pending_rewind: Option<(u64, RewindCheckpoint)>,
    pub(in crate::engine) transient_cause: Option<RequestId>,
    pub(in crate::engine) poisoned: bool,
    pub(in crate::engine) closing: bool,
    pub(in crate::engine) unsettled: Option<String>,
    pub(in crate::engine) tasks: task_ownership::ActorTasks,
    pub(in crate::engine) control: Arc<super::control::SessionControl>,
    pub(in crate::engine) client_roles: BTreeMap<String, ClientRole>,
    pub(in crate::engine) pending_questions: BTreeMap<String, PendingQuestion>,
    pub(in crate::engine) pending_model_switches: BTreeMap<String, PendingModelSwitch>,
    pub(in crate::engine) next_question: u64,
    pub(in crate::engine) accounting: crate::engine::SessionAccountingState,
    pub(in crate::engine) budgeter: Budgeter,
    pub(in crate::engine) model_alias: String,
    pub(in crate::engine) provider: Option<String>,
    pub(in crate::engine) thinking: ThinkingLevel,
    pub(in crate::engine) mode: SessionMode,
    pub(in crate::engine) mode_id: ModeId,
    pub(in crate::engine) pending_plan: Option<PlanArtifact>,
    pub(in crate::engine) approved_plan: Option<PlanArtifact>,
    pub(in crate::engine) plan_gate_active: bool,
    pub(in crate::engine) active_shell: Option<RecoveredUserShell>,
    pub(in crate::engine) initialization_running: bool,
}

pub(in crate::engine) struct PendingQuestion {
    pub(in crate::engine) questions: Vec<rw_types::Question>,
    pub(in crate::engine) _admission: tokio::sync::OwnedSemaphorePermit,
    pub(in crate::engine) turn: u64,
    pub(in crate::engine) respond: oneshot::Sender<Result<String, rw_tools::ToolError>>,
}

pub(in crate::engine) enum PrecommittedAnswer {
    Turn(PendingQuestion, String),
    Model(PendingModelSwitch, ModelContextTransfer),
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::engine) struct PendingModelSwitch {
    pub(in crate::engine) questions: Vec<rw_types::Question>,
    pub(in crate::engine) turn: u64,
    pub(in crate::engine) model: ModelAlias,
    pub(in crate::engine) provider: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::engine) struct PreparedModelSwitch {
    pub(in crate::engine) model: ModelAlias,
    pub(in crate::engine) provider: Option<String>,
    pub(in crate::engine) thinking: ThinkingLevel,
}

pub(in crate::engine) struct PendingApproval {
    pub(in crate::engine) respond: oneshot::Sender<ApprovalDecision>,
    pub(in crate::engine) binding: Option<ApprovalBinding>,
    pub(in crate::engine) request: PermissionRequest,
    pub(in crate::engine) turn: u64,
}

impl ActorState {
    pub(super) fn recover(
        session_id: SessionId,
        event_clock: Arc<dyn EventClock>,
        default_model_alias: &str,
        default_thinking: ThinkingLevel,
        modes: &ModeRegistry,
        recovered: SessionActorRecovery,
        control: Arc<super::control::SessionControl>,
    ) -> Self {
        let pending_model_switches = recovered
            .pending_questions
            .iter()
            .filter_map(|(question_id, recovered)| {
                recovered
                    .questions
                    .iter()
                    .find_map(|question| question.model_switch.as_ref())
                    .map(|target| {
                        (
                            question_id.clone(),
                            PendingModelSwitch {
                                questions: recovered.questions.clone(),
                                turn: recovered.agent_turn,
                                model: target.model.clone(),
                                provider: target.provider.clone(),
                            },
                        )
                    })
            })
            .collect();
        let queued_positions = recovered
            .queued_messages
            .iter()
            .enumerate()
            .map(|(index, _)| {
                recovered
                    .queued_message_positions
                    .get(index)
                    .copied()
                    .unwrap_or_else(|| u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1))
            })
            .collect();
        let mode_id = recovered
            .mode_id
            .unwrap_or_else(|| ModeId(session_mode_name(recovered.mode).to_owned()));
        let mode = modes
            .get(&mode_id.0)
            .map_or(recovered.mode, mode_permission_base);
        Self {
            live: super::live_state::LiveState {
                controls_source: None,
                budget: recovered.latest_budget,
                plugin_statuses: recovered.plugin_statuses,
                ..Default::default()
            },
            session_id,
            title_generation_started: recovered.title.is_some(),
            session_title: recovered.title,
            event_clock,
            resolved_model: recovered.conversation.resolved_model,
            conversation_turns: recovered.conversation.turns,
            system_turns: recovered.conversation.system_turns,
            system_resolved_model: recovered.conversation.system_resolved_model,
            title_prompt: recovered.conversation.title_prompt,
            has_assistant_text: recovered.conversation.has_assistant_text,
            approved_plan_item: recovered.conversation.approved_plan_item,
            _recovery_source: recovered.source,
            suspended_inputs: None,
            queued: recovered.queued_messages.into_iter().collect(),
            queued_positions,
            running: None,
            pending_command: None,
            pending_context_read: None,
            pending_plugin_tool: None,
            pending_model_preparation: None,
            pending_approvals: BTreeMap::new(),
            next_turn: recovered
                .next_turn
                .max(recovered.completed_turns.saturating_add(1))
                .max(1),
            completed_turns: recovered.completed_turns,
            sequence: recovered.last_sequence.map(|sequence| sequence.0),
            pending_rewind: None,
            transient_cause: None,
            poisoned: false,
            closing: false,
            unsettled: None,
            tasks: task_ownership::ActorTasks::default(),
            control,
            client_roles: BTreeMap::new(),
            pending_questions: BTreeMap::new(),
            pending_model_switches,
            next_question: 0,
            accounting: recovered.accounting,
            budgeter: recovered.budgeter,
            model_alias: recovered
                .model_alias
                .unwrap_or_else(|| default_model_alias.to_owned()),
            provider: recovered.provider,
            thinking: recovered.thinking.unwrap_or(default_thinking),
            mode,
            mode_id,
            pending_plan: recovered.pending_plan,
            approved_plan: recovered.approved_plan,
            plan_gate_active: recovered.plan_gate_active,
            active_shell: recovered.active_shell,
            initialization_running: false,
        }
    }

    pub(in crate::engine) fn caused_by(&self) -> Option<RequestId> {
        self.transient_cause.clone().or_else(|| {
            self.running
                .as_ref()
                .and_then(|running| running.caused_by.clone())
        })
    }
}

#[cfg(test)]
mod tests;

impl ActorState {
    pub(in crate::engine) fn has_conversation_context(&self) -> bool {
        self.conversation_turns > self.system_turns
    }

    pub(in crate::engine) fn conversation_summary(&self) -> super::ConversationSummary {
        super::ConversationSummary {
            turns: self.conversation_turns,
            system_turns: self.system_turns,
            resolved_model: self.resolved_model.clone(),
            system_resolved_model: self.system_resolved_model.clone(),
            title_prompt: self.title_prompt.clone(),
            has_assistant_text: self.has_assistant_text,
            approved_plan_item: self.approved_plan_item.clone(),
        }
    }
    pub(in crate::engine) fn replace_conversation(&mut self, summary: super::ConversationSummary) {
        self.conversation_turns = summary.turns;
        self.system_turns = summary.system_turns;
        self.resolved_model = summary.resolved_model;
        self.system_resolved_model = summary.system_resolved_model;
        self.title_prompt = summary.title_prompt;
        self.has_assistant_text = summary.has_assistant_text;
        self.approved_plan_item = summary.approved_plan_item;
    }
    /// Consume a committed body into scalar metadata, releasing its allocation here.
    pub(in crate::engine) fn append_conversation(
        &mut self,
        turn: Turn,
        source: rw_types::SequenceId,
    ) {
        if let Some(model) = turn.meta.model.as_ref().filter(|model| model.contains('/')) {
            self.resolved_model = Some(model.clone());
            if turn.role == rw_types::Role::System {
                self.system_resolved_model = Some(model.clone());
            }
        }
        if turn.role == rw_types::Role::System {
            self.system_turns = self.system_turns.saturating_add(1);
        }
        if self.title_prompt.is_none() {
            self.title_prompt = crate::engine::turn::title::first_meaningful_user_prompt(
                std::slice::from_ref(&turn),
            );
        }
        self.has_assistant_text |=
            crate::engine::turn::title::has_successful_assistant_text(std::slice::from_ref(&turn));
        if crate::engine::projection::approved_plan_context_item(std::slice::from_ref(&turn))
            .is_some()
        {
            self.approved_plan_item = Some(rw_types::context_source::conversation_item(source));
        }
        self.conversation_turns = self.conversation_turns.saturating_add(1);
        drop(turn);
    }
    pub(in crate::engine) fn clear_conversation_except_system(&mut self) {
        self.conversation_turns = self.system_turns;
        self.resolved_model.clone_from(&self.system_resolved_model);
        self.title_prompt = None;
        self.has_assistant_text = false;
        self.approved_plan_item = None;
    }
}
