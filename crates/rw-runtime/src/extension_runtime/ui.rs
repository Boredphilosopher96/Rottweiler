//! One bounded live UI registry. Historical tool surfaces belong to their result
//! event, while panel replacements retire the previous retained value immediately.
mod budget;
pub(crate) mod source;
#[cfg(test)]
mod tests;
use super::{PluginSessionCommand, SharedPluginRedactor};
use budget::{PREPARATION_BYTES, shrink};
pub(crate) use budget::{UiBudget, UiSessionBudget};
use rw_core::{
    AgentLoopError,
    ui::{BoundUiCommand, UiRegistry},
};
use rw_ext::{
    CommandDescriptor, CommandRegistry, PluginEndpoint, PluginRpcError, RpcCommandAdapter,
};
use rw_types::{
    allocation::{AllocationPlan, PreparedAllocation},
    extension_ui::{
        MAX_UI_CONTRIBUTIONS, MAX_UI_DESCRIPTOR_BYTES, UiActionRequest, UiActionTarget, UiCatalog,
        UiCatalogEntry, UiContribution, UiContributionOwner, UiDisplayDescriptor, UiPanelSnapshot,
        UiPanels, UiPresentation,
    },
};
use serde::Serialize;
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::OwnedSemaphorePermit;

use rw_types::extension_ui::{MAX_UI_PANEL_SLOTS, MAX_UI_PANELS_BYTES};
const MAX_PANEL_INPUT_BYTES: usize = 64 * 1024;
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(250);
struct Registration {
    endpoint: Arc<dyn PluginEndpoint>,
    catalog: PreparedAllocation<UiCatalog>,
    encoded: usize,
    _permit: OwnedSemaphorePermit,
}
struct Panel {
    wire: budget::PanelCredit,
    surface: PreparedAllocation<UiPanelSnapshot>,
    encoded: usize,
    permit: OwnedSemaphorePermit,
}
struct State {
    registrations: Vec<Arc<Registration>>,
    panels: Vec<Panel>,
    last_update: Option<Instant>,
    next_revision: u32,
    base: Option<OwnedSemaphorePermit>,
    closed: bool,
}
pub(crate) struct RuntimeUiRegistry {
    budget: Arc<UiBudget>,
    session_budget: Arc<UiSessionBudget>,
    redactor: Arc<SharedPluginRedactor>,
    state: Mutex<State>,
}
impl RuntimeUiRegistry {
    pub(crate) fn new(
        budget: Arc<UiBudget>,
        redactor: Arc<SharedPluginRedactor>,
        session_budget: Arc<UiSessionBudget>,
    ) -> Self {
        Self {
            budget,
            session_budget,
            redactor,
            state: Mutex::new(State {
                registrations: Vec::new(),
                panels: Vec::new(),
                last_update: None,
                next_revision: 1,
                base: None,
                closed: false,
            }),
        }
    }
    pub(crate) fn register(&self, endpoint: Arc<dyn PluginEndpoint>) -> Result<(), PluginRpcError> {
        let declarations = &endpoint.metadata().manifest().capabilities.ui;
        if declarations.is_empty() {
            return Ok(());
        }
        let mut permit = self.budget.prepare()?;
        let owner = endpoint.metadata().ui_owner();
        let mut entries = Vec::with_capacity(declarations.len());
        for declaration in declarations {
            let mut descriptor =
                UiDisplayDescriptor::from_declaration(declaration).map_err(error)?;
            self.redact_descriptor(&mut descriptor)?;
            entries.push(UiCatalogEntry {
                owner: owner.clone(),
                descriptor,
            });
        }
        let catalog = UiCatalog { entries };
        catalog.validate().map_err(error)?;
        let encoded = catalog.entries.iter().try_fold(0usize, |sum, entry| {
            encoded_bytes(entry, MAX_UI_DESCRIPTOR_BYTES).map(|bytes| sum + bytes + 1)
        })?;
        let plan =
            AllocationPlan::new(catalog).map_err(|_| error("UI catalog allocation overflow"))?;
        shrink(
            &mut permit,
            plan.bytes() + std::mem::size_of::<Registration>(),
        )?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_open(&state)?;
        if state
            .registrations
            .iter()
            .any(|entry| entry.endpoint.metadata().ui_owner() == owner)
        {
            return Err(error("duplicate UI generation"));
        }
        let count: usize = state
            .registrations
            .iter()
            .map(|entry| entry.catalog.value().entries.len())
            .sum();
        let bytes: usize = state.registrations.iter().map(|entry| entry.encoded).sum();
        if count + plan.value().entries.len() > MAX_UI_CONTRIBUTIONS
            || bytes + encoded + 32 > MAX_UI_DESCRIPTOR_BYTES
        {
            return Err(error("session UI catalog capacity exhausted"));
        }
        if state.base.is_none() {
            let base = self.budget.base()?;
            state.registrations = Vec::with_capacity(MAX_UI_CONTRIBUTIONS);
            state.panels = Vec::with_capacity(MAX_UI_PANEL_SLOTS);
            state.base = Some(base);
        }
        state.registrations.push(Arc::new(Registration {
            endpoint,
            catalog: plan.prepare(),
            encoded,
            _permit: permit,
        }));
        Ok(())
    }
    pub(crate) fn publish_panel(
        &self,
        owner: &UiContributionOwner,
        id: &str,
        mut data: serde_json::Value,
    ) -> Result<u32, PluginRpcError> {
        encoded_bytes(&data, MAX_PANEL_INPUT_BYTES)?;
        let plan =
            AllocationPlan::new(data).map_err(|_| error("panel input allocation overflow"))?;
        if plan.bytes() > PREPARATION_BYTES / 2 {
            return Err(error("panel input allocation limit"));
        }
        let mut permit = self.budget.prepare()?;
        data = plan.prepare().into_inner();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_open(&state)?;
        let now = Instant::now();
        if state
            .last_update
            .is_some_and(|last| now.saturating_duration_since(last) < MIN_UPDATE_INTERVAL)
        {
            return Err(error("panel update rate exhausted"));
        }
        let registration = find_registration(&state, owner)?;
        let declaration = registration
            .endpoint
            .metadata()
            .manifest()
            .capabilities
            .ui
            .iter()
            .find(|entry| entry.id() == id && matches!(entry, UiContribution::Panel { .. }))
            .ok_or_else(|| error("panel is not declared"))?;
        self.redact_data(&mut data)?;
        let mut presentation =
            UiPresentation::project(owner.clone(), declaration, &data).map_err(error)?;
        self.redact_descriptor(&mut presentation.descriptor)?;
        presentation.validate().map_err(error)?;
        let revision = state.next_revision;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| error("panel revision space exhausted"))?;
        drop(data);
        let snapshot = UiPanelSnapshot {
            revision,
            presentation,
        };
        let encoded = encoded_bytes(&snapshot, 64 * 1024 + 64)?;
        let plan = AllocationPlan::new(snapshot).map_err(|_| error("panel allocation overflow"))?;
        shrink(&mut permit, plan.bytes() + std::mem::size_of::<Panel>())?;
        let existing = state.panels.iter().position(|panel| {
            panel.surface.value().presentation.owner == *owner
                && panel.surface.value().presentation.descriptor.id == id
        });
        if existing.is_none() && state.panels.len() == MAX_UI_PANEL_SLOTS {
            return Err(error("panel capacity exhausted"));
        }
        let retained: usize = state
            .panels
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != existing)
            .map(|(_, panel)| panel.encoded + 1)
            .sum();
        if retained + encoded + 32 > MAX_UI_PANELS_BYTES {
            return Err(error("panel surface capacity exhausted"));
        }
        let prepared = plan.prepare();
        if let Some(index) = existing {
            let panel = &mut state.panels[index];
            panel.wire.resize(encoded + 64)?;
            panel.surface = prepared;
            panel.permit = permit;
            panel.encoded = encoded;
        } else {
            let wire = self.session_budget.panel(encoded + 64)?;
            state.panels.push(Panel {
                wire,
                surface: prepared,
                encoded,
                permit,
            });
        }
        state.next_revision = next_revision;
        state.last_update = Some(now);
        Ok(revision)
    }
    pub(crate) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.registrations = Vec::new();
        state.panels = Vec::new();
        state.base.take();
    }
    fn redact_descriptor(
        &self,
        descriptor: &mut UiDisplayDescriptor,
    ) -> Result<(), PluginRpcError> {
        use rw_types::extension_ui::UiDisplayField;
        self.redact_label(&mut descriptor.title)?;
        for field in &mut descriptor.fields {
            match field {
                UiDisplayField::Text { label, .. }
                | UiDisplayField::Badge { label, .. }
                | UiDisplayField::List { label, .. } => self.redact_label(label)?,
                UiDisplayField::Table { label, columns, .. } => {
                    self.redact_label(label)?;
                    for column in columns {
                        self.redact_label(column)?;
                    }
                }
            }
        }
        for action in &mut descriptor.actions {
            self.redact_label(&mut action.label)?;
        }
        Ok(())
    }
    fn redact_label(&self, text: &mut String) -> Result<(), PluginRpcError> {
        let redactor = self
            .redactor
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *text = redactor.redact_text_bounded(text, 1024).map_err(error)?;
        let mut end = text.len().min(128);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        Ok(())
    }
    fn redact_data(&self, data: &mut serde_json::Value) -> Result<(), PluginRpcError> {
        fn visit(
            data: &mut serde_json::Value,
            redactor: &rw_providers::FixtureRedactor,
            bytes: &mut usize,
        ) -> Result<(), PluginRpcError> {
            match data {
                serde_json::Value::String(text) => {
                    *text = redactor.redact_text_bounded(text, *bytes).map_err(error)?;
                    *bytes -= text.len();
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        visit(value, redactor, bytes)?;
                    }
                }
                serde_json::Value::Object(values) => {
                    for value in values.values_mut() {
                        visit(value, redactor, bytes)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        let redactor = self
            .redactor
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        visit(data, &redactor, &mut (256 * 1024))
    }
}
impl UiRegistry for RuntimeUiRegistry {
    fn owns(&self, owner: &UiContributionOwner) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.closed && find_registration(&state, owner).is_ok()
    }
    fn catalog(&self) -> Result<UiCatalog, AgentLoopError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_open(&state).map_err(|error| core_error(&error))?;
        Ok(UiCatalog {
            entries: state
                .registrations
                .iter()
                .flat_map(|entry| entry.catalog.value().entries.iter().cloned())
                .collect(),
        })
    }
    fn panels(&self) -> Result<UiPanels, AgentLoopError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_open(&state).map_err(|error| core_error(&error))?;
        Ok(UiPanels {
            panels: state
                .panels
                .iter()
                .map(|panel| panel.surface.value().clone())
                .collect(),
        })
    }
    fn resolve_action(
        &self,
        request: &UiActionRequest,
        tool: Option<&UiPresentation>,
    ) -> Result<BoundUiCommand, AgentLoopError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_open(&state).map_err(|error| core_error(&error))?;
        let registration =
            find_registration(&state, &request.owner).map_err(|error| core_error(&error))?;
        let presentation = match &request.target {
            UiActionTarget::Tool { .. } => tool,
            UiActionTarget::Panel { revision } => state
                .panels
                .iter()
                .map(|panel| panel.surface.value())
                .find(|panel| {
                    panel.revision == *revision
                        && panel.presentation.owner == request.owner
                        && panel.presentation.descriptor.id == request.contribution_id
                })
                .map(|panel| &panel.presentation),
        }
        .ok_or_else(|| core_error(&error("UI action source is unavailable")))?;
        if presentation.owner != request.owner
            || presentation.descriptor.id != request.contribution_id
            || !presentation
                .descriptor
                .actions
                .iter()
                .any(|action| action.id == request.action_id)
        {
            return Err(core_error(&error("UI action source identity differs")));
        }
        let declaration = registration
            .endpoint
            .metadata()
            .manifest()
            .capabilities
            .ui
            .iter()
            .find(|entry| entry.id() == request.contribution_id)
            .ok_or_else(|| core_error(&error("UI contribution is not declared")))?;
        if !matches!(
            (&request.target, declaration),
            (UiActionTarget::Tool { .. }, UiContribution::Tool { .. })
                | (UiActionTarget::Panel { .. }, UiContribution::Panel { .. })
        ) {
            return Err(core_error(&error("UI action target kind differs")));
        }
        let action = declaration
            .actions()
            .iter()
            .find(|action| action.id == request.action_id)
            .ok_or_else(|| core_error(&error("UI action is not declared")))?;
        let mut commands = CommandRegistry::new();
        commands
            .register(
                CommandDescriptor::new(&action.command, ""),
                PluginSessionCommand {
                    inner: RpcCommandAdapter::new(&action.command, registration.endpoint.clone()),
                },
            )
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        let arguments = serde_json::to_string(&action.arguments)
            .map_err(|_| core_error(&error("UI action arguments are invalid")))?;
        commands
            .bind(&action.command, arguments)
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))
    }
}
fn find_registration<'a>(
    state: &'a State,
    owner: &UiContributionOwner,
) -> Result<&'a Registration, PluginRpcError> {
    state
        .registrations
        .iter()
        .find(|entry| entry.endpoint.metadata().ui_owner() == *owner)
        .map(AsRef::as_ref)
        .ok_or_else(|| error("UI generation is unavailable"))
}
fn ensure_open(state: &State) -> Result<(), PluginRpcError> {
    if state.closed {
        Err(error("UI registry is closed"))
    } else {
        Ok(())
    }
}
fn encoded_bytes(value: &impl Serialize, limit: usize) -> Result<usize, PluginRpcError> {
    struct Count {
        bytes: usize,
        limit: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|bytes| *bytes <= self.limit)
                .ok_or_else(|| std::io::Error::other("UI encoded limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { bytes: 0, limit };
    serde_json::to_writer(&mut count, value).map_err(error)?;
    Ok(count.bytes)
}
fn core_error(error: &PluginRpcError) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(error.to_string())
}
fn error(message: impl std::fmt::Display) -> PluginRpcError {
    PluginRpcError {
        code: "ui_unavailable".into(),
        message: message.to_string(),
    }
}
