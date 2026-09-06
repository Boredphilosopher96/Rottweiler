//! Durable mutation identity is independent of a transport's client binding.
use super::{
    CachedDispatch, ClientCommand, ClientId, EngineHost, HostError, command_ack, host_error_code,
    rejected,
};
use rw_types::command_receipt::{CommandReceipt, ReceiptAdmission};

impl EngineHost {
    pub(super) async fn execute(
        &self,
        mut command: ClientCommand,
        payload_hash: String,
    ) -> CachedDispatch {
        if !durable_mutation(&command) {
            return self.execute_command(command, payload_hash).await;
        }
        let client = std::mem::replace(&mut command.meta_mut().client_id, ClientId(String::new()));
        let fingerprint = super::read::command_hash(&command);
        command.meta_mut().client_id = client;
        let meta = command.meta().clone();
        let result = async {
            let fingerprint = fingerprint.map_err(|_| HostError::Protocol("command fingerprint is invalid".into()))?;
            match self.factory.admit_command_receipt(&command, &fingerprint).await? {
                ReceiptAdmission::Completed(mut receipt) => {
                    for event in &mut receipt.events {
                        if let Some(ack) = event.command_meta_mut() {
                            ack.client_id.clone_from(&meta.client_id);
                            ack.request_id.clone_from(&meta.request_id);
                        }
                    }
                    Ok(CachedDispatch { outcome: receipt.outcome, events: receipt.events, cacheable: true })
                }
                ReceiptAdmission::Indeterminate => {
                    let outcome = rejected("operation_indeterminate", "operation is admitted but has no proven completion; inspect session state before further actions");
                    Ok(CachedDispatch { events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)], outcome, cacheable: false })
                }
                ReceiptAdmission::Admitted => {
                    let dispatch = self.execute_command(command, payload_hash).await;
                    let receipt = self.factory.complete_command_receipt(&meta.request_id, &fingerprint,
                        CommandReceipt { outcome: dispatch.outcome, events: dispatch.events }).await?;
                    Ok(CachedDispatch { outcome: receipt.outcome, events: receipt.events, cacheable: true })
                }
            }
        }.await;
        result.unwrap_or_else(|error| {
            let outcome = rejected(host_error_code(&error), &error.to_string());
            CachedDispatch {
                events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                outcome,
                cacheable: true,
            }
        })
    }
}
fn durable_mutation(command: &ClientCommand) -> bool {
    match command {
        ClientCommand::CreateSession { .. }
        | ClientCommand::InvokeUiAction { .. }
        | ClientCommand::SendMessage { .. }
        | ClientCommand::ApproveTool { .. }
        | ClientCommand::ApprovePlan { .. }
        | ClientCommand::AnswerQuestion { .. }
        | ClientCommand::SwitchMode { .. }
        | ClientCommand::SwitchModel { .. }
        | ClientCommand::Compact { .. }
        | ClientCommand::Rewind { .. }
        | ClientCommand::UserShellStarted { .. }
        | ClientCommand::UserShellEnded { .. }
        | ClientCommand::PinContext { .. }
        | ClientCommand::EvictContext { .. }
        | ClientCommand::ReviewFile { .. }
        | ClientCommand::SetSetting { .. }
        | ClientCommand::AddMcpHttpServer { .. }
        | ClientCommand::AddMcpStdioServer { .. }
        | ClientCommand::RemoveMcpServer { .. }
        | ClientCommand::ApproveMcpServer { .. }
        | ClientCommand::SetMcpServerEnabled { .. }
        | ClientCommand::AddSessionPermissionRule { .. }
        | ClientCommand::RemoveSessionPermissionRule { .. }
        | ClientCommand::RemoveQueuedMessage { .. }
        | ClientCommand::ClearQueuedMessages { .. }
        | ClientCommand::RenameSession { .. }
        | ClientCommand::ExportSession { .. }
        | ClientCommand::RevokePermissionApproval { .. }
        | ClientCommand::ConfigureBuiltinProvider { .. }
        | ClientCommand::ContinueSubagent { .. }
        | ClientCommand::CloseSubagent { .. }
        | ClientCommand::ResolveChildControl { .. } => true,
        ClientCommand::GetSessionState { .. }
        | ClientCommand::GetSessionControls { .. }
        | ClientCommand::ReadFamilyControls { .. }
        | ClientCommand::ResolveChildReadScope { .. }
        | ClientCommand::ReadChildState { .. }
        | ClientCommand::ReadChildControls { .. }
        | ClientCommand::GetUiCatalog { .. }
        | ClientCommand::GetUiPanels { .. }
        | ClientCommand::ReadSessionChildren { .. }
        | ClientCommand::GetTodos { .. }
        | ClientCommand::ReadTranscriptTail { .. }
        | ClientCommand::ReadTranscript { .. }
        | ClientCommand::ReadTranscriptContent { .. }
        | ClientCommand::ResumeSession { .. }
        | ClientCommand::AttachSession { .. }
        | ClientCommand::Interrupt { .. }
        | ClientCommand::Fork { .. }
        | ClientCommand::TakeDriver { .. }
        | ClientCommand::AttachDevelopmentPlugin { .. }
        | ClientCommand::DetachDevelopmentPlugin { .. }
        | ClientCommand::GetContext { .. }
        | ClientCommand::GetCost { .. }
        | ClientCommand::GetSessionReview { .. }
        | ClientCommand::DumpPrompt { .. }
        | ClientCommand::ListSessions { .. }
        | ClientCommand::SearchSessions { .. }
        | ClientCommand::ListCommands { .. }
        | ClientCommand::ListModes { .. }
        | ClientCommand::ListModels { .. }
        | ClientCommand::ListSettings { .. }
        | ClientCommand::ListMcpServers { .. }
        | ClientCommand::ListRuntimeServices { .. }
        | ClientCommand::ReviewMcpServer { .. }
        | ClientCommand::ListPermissions { .. }
        | ClientCommand::BeginProviderAuth { .. }
        | ClientCommand::CompleteProviderAuth { .. }
        | ClientCommand::CancelProviderAuth { .. }
        | ClientCommand::SearchWorkspaceFiles { .. }
        | ClientCommand::PreviewWorkspaceFile { .. }
        | ClientCommand::GetWorkspaceStatus { .. }
        | ClientCommand::GetWorkspaceDiff { .. }
        | ClientCommand::ListSubagents { .. }
        | ClientCommand::InterruptSubagent { .. }
        | ClientCommand::ShutdownHost { .. } => false,
    }
}
