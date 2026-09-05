mod envelope;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self};
use std::path::{Path, PathBuf};

use rw_types::attachment_contract::{
    MAX_ATTACHMENTS_PER_MESSAGE, MAX_IMAGE_ATTACHMENT_BYTES, MAX_TEXT_ATTACHMENT_BYTES,
    MAX_TOTAL_ATTACHMENT_BYTES,
};
use rw_types::config::{PermissionDecision, ThinkingLevel};
use rw_types::extension_contract::{
    ExtensionDeliveryCursor, ExtensionStateMutation, ExtensionStateTransaction,
};
use rw_types::extension_ui::{
    UiAction, UiActionRequest, UiActionTarget, UiCatalog, UiCatalogEntry, UiContribution,
    UiContributionOwner, UiDisplayAction, UiDisplayDescriptor, UiDisplayField, UiDisplaySurface,
    UiField, UiGenerationId, UiPanelSnapshot, UiPanels, UiPresentation, UiProjectedField,
    UiProjectedFields, UiSelectorStep, UiTableColumn,
};
use rw_types::todo::{TodoItem, TodoReadResult, TodoReadSnapshot, TodoSnapshot, TodoStatus};
use rw_types::{
    AccountingAttribution, Answer, ApprovalBinding, ApprovalDecision, Attachment, AttachmentData,
    Block, BudgetLevel, BudgetScope, BudgetUnit, CacheBreakpoint, ClientCommand, ClientId,
    ClientRole, CommandAckMeta, CommandDescriptor, CommandMeta, CommandOutcome, CommandSource,
    CompactionReason, ContextItemId, ContextItemKind, ContextItemSnapshot, ContextItemState,
    ContextSnapshot, Cost, CostSnapshot, DiffArtifact, EngineError, EngineErrorCategory,
    EngineEvent, EventMeta, ImageRef, MAX_MCP_SERVER_ID_BYTES, MCP_SERVER_ID_PATTERN,
    McpApprovalReview, McpEnvironmentEntry, McpServerDescriptor, McpServerState, ModeDescriptor,
    ModeId, ModelAlias, ModelAliasDescriptor, ModelCacheBehavior, ModelCapabilities,
    ModelCatalogSnapshot, ModelContextTransfer, ModelDescriptor, ModelSwitchQuestion,
    PermissionApprovalDescriptor, PermissionApprovalScope, PermissionModeDescriptor,
    PermissionRuleDescriptor, PermissionStateDescriptor, PlanArtifact, PlanDecision, PlanStep,
    ProgressAmount, PromptDump, PromptTool, ProviderAuthAttemptId, ProviderAuthChallenge,
    ProviderAuthKind, ProviderCallActuals, ProviderCallIdentity, ProviderDescriptor,
    ProviderNextAction, Question, QuestionId, QuestionOption, QuestionResponseKind, RequestId,
    ReviewFileDecision, ReviewFileStatus, RewindSourcePosition, RewindTarget, Role,
    RuntimeServiceDescriptor, RuntimeServiceKind, SequenceId, SessionDescriptor, SessionId,
    SessionReview, SessionReviewFile, ShellId, StoredAttachment, SubagentActivity,
    SubagentDescriptor, SubagentId, SubagentIsolation, SubagentResult, SubagentStatus,
    TRANSIENT_ENGINE_EVENT_TYPES, ToolCallId, ToolCapability, ToolInvocationId, ToolOutput,
    ToolOutputPart, ToolOutputStream, ToolProgress, TouchedFile, TouchedFileStatus,
    TranscriptFormat, Turn, TurnAccounting, TurnId, TurnMeta, TurnStatus, UnifiedDiff,
    UnrestorablePath, Usage, UserSettingDescriptor, WorkspaceDiff, WorkspaceFileMatch,
    WorkspaceFilePreview, WorkspaceRootDescriptor, WorkspaceStatus,
};
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde_json::json;
use ts_rs::{Config as TypeScriptConfig, TS};

use super::XtaskError;

pub(super) fn run(mut arguments: impl Iterator<Item = String>) -> Result<(), XtaskError> {
    let check = match arguments.next().as_deref() {
        None => false,
        Some("--check") => true,
        Some(_) => return Err(XtaskError::Usage),
    };
    if arguments.next().is_some() {
        return Err(XtaskError::Usage);
    }

    super::plugin_codegen::run(check).map_err(XtaskError::GeneratedContract)?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let protocol = root.join("protocol");
    let artifacts = generated_artifacts()?;
    for (relative_path, contents) in artifacts {
        let path = protocol.join(relative_path);
        if check {
            check_artifact(&path, &contents)?;
        } else {
            write_artifact(&path, &contents)?;
        }
    }
    let mut validator = std::process::Command::new("bun");
    validator
        .current_dir(&root)
        .arg("run")
        .arg("packages/tui/scripts/generate-event-validator.ts");
    if check {
        validator.arg("--check");
    }
    let status = validator.status().map_err(|error| {
        XtaskError::GeneratedContract(format!(
            "could not run engine-event validator generator: {error}"
        ))
    })?;
    if !status.success() {
        return Err(XtaskError::GeneratedContract(
            "engine-event validator generation failed; install TUI dependencies with the pinned Bun version".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ContractFixture {
    turns: Vec<Turn>,
    client_commands: Vec<ClientCommand>,
    engine_events: Vec<EngineEvent>,
}

fn generated_artifacts() -> Result<Vec<(PathBuf, String)>, XtaskError> {
    let fixture = contract_fixture();
    Ok(vec![
        (
            PathBuf::from("session-event-envelope.schema.json"),
            envelope::generate()?,
        ),
        (PathBuf::from("types.ts"), generate_typescript()?),
        (
            PathBuf::from("schema/block.schema.json"),
            generate_schema::<Block>()?,
        ),
        (
            PathBuf::from("schema/tool-output.schema.json"),
            generate_schema::<ToolOutput>()?,
        ),
        (
            PathBuf::from("schema/client-command.schema.json"),
            generate_schema::<ClientCommand>()?,
        ),
        (
            PathBuf::from("schema/engine-event.schema.json"),
            generate_schema::<EngineEvent>()?,
        ),
        (
            PathBuf::from("schema/ui-presentation.schema.json"),
            generate_schema::<UiPresentation>()?,
        ),
        (
            PathBuf::from("schema/command-reply.schema.json"),
            generate_schema::<rw_types::CommandReply>()?,
        ),
        (
            PathBuf::from("fixtures/contract.json"),
            serde_json::to_string_pretty(&fixture)? + "\n",
        ),
        (
            PathBuf::from("fixtures/contract.ts"),
            generate_typescript_fixture(&fixture)?,
        ),
    ])
}

fn generate_typescript_fixture(fixture: &ContractFixture) -> Result<String, serde_json::Error> {
    let fixture_json = serde_json::to_string_pretty(fixture)?;
    Ok(format!(
        "// @generated by `cargo xtask codegen`; do not edit by hand.\n\nimport type {{ ClientCommand, EngineEvent, Turn }} from \"../types\";\n\nexport const contractFixture = {fixture_json} satisfies {{\n  turns: Turn[];\n  client_commands: ClientCommand[];\n  engine_events: EngineEvent[];\n}};\n"
    ))
}

#[allow(clippy::too_many_lines)]
fn generate_typescript() -> Result<String, XtaskError> {
    let mut output =
        String::from("// @generated by `cargo xtask codegen`; do not edit by hand.\n\n");
    output.push_str(
        "export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };\n\n",
    );
    output.push_str("export const PROTOCOL_VERSION = ");
    output.push_str(&rw_types::PROTOCOL_VERSION.to_string());
    output.push_str(" as const;\n\n");
    output.push_str("export const TRANSCRIPT_PROJECTION_VERSION = ");
    output.push_str(&rw_types::transcript::TRANSCRIPT_PROJECTION_VERSION.to_string());
    output.push_str(" as const;\n\n");
    for (name, value) in [
        ("MAX_ATTACHMENTS_PER_MESSAGE", MAX_ATTACHMENTS_PER_MESSAGE),
        ("MAX_TEXT_ATTACHMENT_BYTES", MAX_TEXT_ATTACHMENT_BYTES),
        ("MAX_IMAGE_ATTACHMENT_BYTES", MAX_IMAGE_ATTACHMENT_BYTES),
        ("MAX_TOTAL_ATTACHMENT_BYTES", MAX_TOTAL_ATTACHMENT_BYTES),
        ("MAX_MCP_SERVER_ID_BYTES", MAX_MCP_SERVER_ID_BYTES),
        ("MAX_COMMAND_REPLY_BYTES", rw_types::MAX_COMMAND_REPLY_BYTES),
        ("MAX_CLIENT_READS", rw_types::MAX_CLIENT_READS),
        ("MAX_CLIENT_CONTROLS", rw_types::MAX_CLIENT_CONTROLS),
        (
            "MAX_UI_CONTRIBUTIONS",
            rw_types::extension_ui::MAX_UI_CONTRIBUTIONS,
        ),
        (
            "MAX_UI_DESCRIPTOR_BYTES",
            rw_types::extension_ui::MAX_UI_DESCRIPTOR_BYTES,
        ),
        ("MAX_UI_FIELDS", rw_types::extension_ui::MAX_UI_FIELDS),
        ("MAX_UI_ACTIONS", rw_types::extension_ui::MAX_UI_ACTIONS),
        (
            "MAX_UI_LABEL_BYTES",
            rw_types::extension_ui::MAX_UI_LABEL_BYTES,
        ),
        (
            "MAX_UI_VALUE_BYTES",
            rw_types::extension_ui::MAX_UI_VALUE_BYTES,
        ),
        (
            "MAX_UI_LIST_ITEMS",
            rw_types::extension_ui::MAX_UI_LIST_ITEMS,
        ),
        (
            "MAX_UI_TABLE_ROWS",
            rw_types::extension_ui::MAX_UI_TABLE_ROWS,
        ),
        (
            "MAX_UI_TABLE_COLUMNS",
            rw_types::extension_ui::MAX_UI_TABLE_COLUMNS,
        ),
        (
            "MAX_UI_PANEL_SLOTS",
            rw_types::extension_ui::MAX_UI_PANEL_SLOTS,
        ),
        (
            "MAX_UI_PANELS_BYTES",
            rw_types::extension_ui::MAX_UI_PANELS_BYTES,
        ),
        (
            "MAX_UI_SURFACE_BYTES",
            rw_types::extension_ui::MAX_UI_SURFACE_BYTES,
        ),
        (
            "MAX_PENDING_TOOL_INVOCATIONS",
            rw_types::tool_admission::MAX_PENDING_TOOL_INVOCATIONS,
        ),
        (
            "MAX_PENDING_TOOL_ARGUMENT_BYTES",
            rw_types::tool_admission::MAX_PENDING_TOOL_ARGUMENT_BYTES,
        ),
        (
            "MAX_PENDING_TOOL_PREPARED_BYTES",
            rw_types::tool_admission::MAX_PENDING_TOOL_PREPARED_BYTES,
        ),
        (
            "MAX_TOOL_CALL_ID_BYTES",
            rw_types::tool_admission::MAX_TOOL_CALL_ID_BYTES,
        ),
        (
            "MAX_TOOL_NAME_BYTES",
            rw_types::tool_admission::MAX_TOOL_NAME_BYTES,
        ),
        ("MAX_TODO_ITEMS", rw_types::todo::MAX_TODO_ITEMS),
        ("MAX_TODO_ID_BYTES", rw_types::todo::MAX_TODO_ID_BYTES),
        (
            "MAX_TODO_CONTENT_BYTES",
            rw_types::todo::MAX_TODO_CONTENT_BYTES,
        ),
        ("MAX_TODO_TOTAL_BYTES", rw_types::todo::MAX_TODO_TOTAL_BYTES),
    ] {
        output.push_str("export const ");
        output.push_str(name);
        output.push_str(" = ");
        output.push_str(&value.to_string());
        output.push_str(" as const;\n");
    }
    output.push_str("export const MCP_SERVER_ID_PATTERN = ");
    output.push_str(&serde_json::to_string(MCP_SERVER_ID_PATTERN)?);
    output.push_str(" as const;\n");
    output.push('\n');
    let typescript_config = TypeScriptConfig::default();

    macro_rules! declaration {
        ($type:ty) => {{
            output.push_str("export ");
            output.push_str(&<$type as TS>::decl(&typescript_config));
            output.push_str("\n\n");
        }};
    }

    declaration!(ToolCallId);
    declaration!(ToolInvocationId);
    declaration!(ToolProgress);
    declaration!(ExtensionDeliveryCursor);
    declaration!(ExtensionStateMutation);
    declaration!(ExtensionStateTransaction);
    declaration!(UiGenerationId);
    declaration!(UiContributionOwner);
    declaration!(UiDisplayField);
    declaration!(UiDisplayAction);
    declaration!(UiDisplaySurface);
    declaration!(UiDisplayDescriptor);
    declaration!(UiPresentation);
    declaration!(UiCatalogEntry);
    declaration!(UiCatalog);
    declaration!(UiActionTarget);
    declaration!(UiActionRequest);
    declaration!(UiPanelSnapshot);
    declaration!(UiPanels);
    declaration!(UiAction);
    declaration!(UiContribution);
    declaration!(UiField);
    declaration!(UiProjectedField);
    declaration!(UiProjectedFields);
    declaration!(UiSelectorStep);
    declaration!(UiTableColumn);
    declaration!(TodoItem);
    declaration!(TodoStatus);
    declaration!(TodoSnapshot);
    declaration!(TodoReadSnapshot);
    declaration!(TodoReadResult);

    declaration!(ProgressAmount);
    declaration!(SessionId);
    declaration!(ClientId);
    declaration!(RequestId);
    declaration!(TurnId);
    declaration!(ShellId);
    declaration!(QuestionId);
    declaration!(SubagentId);
    declaration!(SubagentIsolation);
    declaration!(SubagentActivity);
    declaration!(SubagentDescriptor);
    declaration!(SubagentStatus);
    declaration!(TouchedFileStatus);
    declaration!(TouchedFile);
    declaration!(DiffArtifact);
    declaration!(SubagentResult);
    declaration!(ContextItemId);
    declaration!(ModelAlias);
    declaration!(SequenceId);
    declaration!(Role);
    declaration!(ImageRef);
    declaration!(ToolOutputPart);
    declaration!(ToolOutput);
    declaration!(Block);
    declaration!(TurnMeta);
    declaration!(Turn);
    declaration!(CommandMeta);
    declaration!(EventMeta);
    declaration!(CommandAckMeta);
    declaration!(ClientRole);
    declaration!(AttachmentData);
    declaration!(Attachment);
    declaration!(StoredAttachment);
    declaration!(SessionDescriptor);
    declaration!(CommandDescriptor);
    declaration!(CommandSource);
    declaration!(ModelCacheBehavior);
    declaration!(ModelCapabilities);
    declaration!(ModelDescriptor);
    declaration!(ModelAliasDescriptor);
    declaration!(ProviderDescriptor);
    declaration!(ProviderAuthKind);
    declaration!(ProviderAuthAttemptId);
    declaration!(ProviderAuthChallenge);
    declaration!(ProviderNextAction);
    declaration!(ModelCatalogSnapshot);
    declaration!(UserSettingDescriptor);
    declaration!(McpServerState);
    declaration!(McpServerDescriptor);
    declaration!(McpApprovalReview);
    declaration!(McpEnvironmentEntry);
    declaration!(RuntimeServiceKind);
    declaration!(RuntimeServiceDescriptor);
    declaration!(WorkspaceFileMatch);
    declaration!(WorkspaceFilePreview);
    declaration!(WorkspaceStatus);
    declaration!(WorkspaceDiff);
    declaration!(WorkspaceRootDescriptor);
    declaration!(UnifiedDiff);
    declaration!(ApprovalBinding);
    declaration!(ApprovalDecision);
    declaration!(ModeId);
    declaration!(ModeDescriptor);
    declaration!(PlanStep);
    declaration!(PlanArtifact);
    declaration!(PlanDecision);
    declaration!(RewindSourcePosition);
    declaration!(RewindTarget);
    declaration!(ReviewFileDecision);
    declaration!(ReviewFileStatus);
    declaration!(SessionReviewFile);
    declaration!(SessionReview);
    declaration!(QuestionResponseKind);
    declaration!(ModelContextTransfer);
    declaration!(ModelSwitchQuestion);
    declaration!(QuestionOption);
    declaration!(Question);
    declaration!(Answer);
    declaration!(ContextItemKind);
    declaration!(ContextItemState);
    declaration!(ContextItemSnapshot);
    declaration!(CacheBreakpoint);
    declaration!(ContextSnapshot);
    declaration!(AccountingAttribution);
    declaration!(ProviderCallIdentity);
    declaration!(ProviderCallActuals);
    declaration!(TurnAccounting);
    declaration!(CostSnapshot);
    declaration!(rw_types::billing::SubscriptionQuotaSummary);
    declaration!(PromptTool);
    declaration!(PromptDump);
    declaration!(PermissionDecision);
    declaration!(PermissionModeDescriptor);
    declaration!(PermissionApprovalScope);
    declaration!(PermissionRuleDescriptor);
    declaration!(PermissionApprovalDescriptor);
    declaration!(PermissionStateDescriptor);
    declaration!(ClientCommand);
    declaration!(TranscriptFormat);
    declaration!(ToolCapability);
    declaration!(ToolOutputStream);
    declaration!(TurnStatus);
    declaration!(ThinkingLevel);
    declaration!(CompactionReason);
    declaration!(BudgetUnit);
    declaration!(BudgetLevel);
    declaration!(BudgetScope);
    declaration!(Usage);
    declaration!(Cost);
    declaration!(UnrestorablePath);
    declaration!(EngineErrorCategory);
    declaration!(EngineError);
    declaration!(CommandOutcome);
    declaration!(EngineEvent);
    declaration!(rw_types::CommandReply);
    declaration!(rw_types::transcript::TranscriptOrdinal);
    declaration!(rw_types::transcript::TranscriptGeneration);
    declaration!(rw_types::transcript::TranscriptView);
    declaration!(rw_types::transcript::TranscriptPosition);
    declaration!(rw_types::transcript::TranscriptRead);
    declaration!(rw_types::transcript::TranscriptItem);
    declaration!(rw_types::transcript::TranscriptInvalidation);
    declaration!(rw_types::transcript::TranscriptAnchor);
    declaration!(rw_types::transcript::TranscriptPage);
    declaration!(rw_types::transcript::TranscriptReadResult);
    declaration!(rw_types::transcript::TranscriptContentRead);
    declaration!(rw_types::transcript::TranscriptContentPage);
    declaration!(rw_types::transcript::TranscriptItemId);
    declaration!(rw_types::transcript::TranscriptContentSelector);
    declaration!(rw_types::transcript::TranscriptContentSource);
    declaration!(rw_types::transcript::TranscriptPreviewFormat);
    declaration!(rw_types::transcript::TranscriptBodyPreview);
    declaration!(rw_types::transcript::TranscriptConversationBlock);
    declaration!(rw_types::transcript::TranscriptToolStatus);
    declaration!(rw_types::transcript::TranscriptToolPresentation);
    declaration!(rw_types::transcript::TranscriptSubagentStatus);
    declaration!(rw_types::transcript::TranscriptContent);

    output.push_str(&generate_engine_event_delivery()?);
    output.push_str(&generate_command_execution()?);
    Ok(output
        .trim_end()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

fn generate_command_execution() -> Result<String, XtaskError> {
    let schema = serde_json::to_value(schema_for!(ClientCommand))?;
    let variants = schema
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| XtaskError::GeneratedContract("ClientCommand has no variants".into()))?;
    let read_tags = serde_json::to_value(ClientCommand::read_type_tags())?;
    let mut reads: BTreeSet<&str> = read_tags
        .as_array()
        .ok_or_else(|| XtaskError::GeneratedContract("read tags must be an array".into()))?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let urgent_tags = serde_json::to_value(ClientCommand::urgent_type_tags())?;
    let mut urgent: BTreeSet<&str> = urgent_tags
        .as_array()
        .ok_or_else(|| XtaskError::GeneratedContract("urgent tags must be an array".into()))?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let mut lanes = BTreeMap::new();
    let mut classes = BTreeMap::new();
    for variant in variants {
        let tag = variant
            .pointer("/properties/type/const")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| XtaskError::GeneratedContract("command tag missing".into()))?;
        classes.insert(tag, if reads.remove(tag) { "read" } else { "control" });
        lanes.insert(
            tag,
            if urgent.remove(tag) {
                "urgent"
            } else {
                "normal"
            },
        );
    }
    if !reads.is_empty() || !urgent.is_empty() {
        return Err(XtaskError::GeneratedContract(
            "read classification has unknown command tags".into(),
        ));
    }
    let mut output = String::from("\nexport const CLIENT_COMMAND_EXECUTION = {\n");
    for (tag, class) in classes {
        use std::fmt::Write as _;
        let _ = writeln!(output, "  {tag}: \"{class}\",");
    }
    output.push_str(
        "} as const satisfies Record<ClientCommand[\"type\"], \"read\" | \"control\">;\n",
    );
    output.push_str("\nexport const CLIENT_COMMAND_LANE = {\n");
    for (tag, lane) in lanes {
        use std::fmt::Write as _;
        let _ = writeln!(output, "  {tag}: \"{lane}\",");
    }
    output.push_str(
        "} as const satisfies Record<ClientCommand[\"type\"], \"normal\" | \"urgent\">;\n",
    );
    Ok(output)
}

fn generate_engine_event_delivery() -> Result<String, XtaskError> {
    let schema = serde_json::to_value(schema_for!(EngineEvent))?;
    let variants = schema
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            XtaskError::GeneratedContract("EngineEvent has no oneOf variants".to_owned())
        })?;
    let transient = TRANSIENT_ENGINE_EVENT_TYPES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut deliveries = BTreeMap::<String, &'static str>::new();
    for variant in variants {
        let properties = variant
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                XtaskError::GeneratedContract("EngineEvent variant has no properties".to_owned())
            })?;
        let event_type = properties
            .get("type")
            .and_then(|property| property.get("const"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                XtaskError::GeneratedContract("EngineEvent variant has no type tag".to_owned())
            })?;
        let meta_type = properties
            .get("meta")
            .and_then(|property| property.get("$ref"))
            .and_then(serde_json::Value::as_str);
        let delivery = if transient.contains(event_type) {
            if meta_type.is_some() {
                return Err(XtaskError::GeneratedContract(format!(
                    "transient event {event_type} unexpectedly has metadata"
                )));
            }
            "transient"
        } else if meta_type.is_some_and(|reference| reference.ends_with("/$defs/EventMeta")) {
            "durable"
        } else {
            "connection"
        };
        if deliveries.insert(event_type.to_owned(), delivery).is_some() {
            return Err(XtaskError::GeneratedContract(format!(
                "duplicate EngineEvent type tag {event_type}"
            )));
        }
    }
    if transient
        .iter()
        .any(|event_type| !deliveries.contains_key(*event_type))
    {
        return Err(XtaskError::GeneratedContract(
            "transient EngineEvent tag is missing from the schema".to_owned(),
        ));
    }
    let mut output = String::from(
        "export type EngineEventDelivery = \"connection\" | \"durable\" | \"transient\";\n\nexport const ENGINE_EVENT_DELIVERY = {\n",
    );
    for (event_type, delivery) in deliveries {
        use std::fmt::Write as _;
        let _ = writeln!(output, "  {event_type}: \"{delivery}\",");
    }
    output.push_str("} as const satisfies Record<EngineEvent[\"type\"], EngineEventDelivery>;\n");
    Ok(output)
}

fn generate_schema<T: JsonSchema>() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&schema_for!(T)).map(|schema| schema + "\n")
}

fn write_artifact(path: &Path, contents: &str) -> Result<(), XtaskError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| XtaskError::Write {
            path: parent.to_owned(),
            source,
        })?;
    }
    fs::write(path, contents).map_err(|source| XtaskError::Write {
        path: path.to_owned(),
        source,
    })
}

fn check_artifact(path: &Path, expected: &str) -> Result<(), XtaskError> {
    let actual = fs::read_to_string(path).map_err(|source| XtaskError::Read {
        path: path.to_owned(),
        source,
    })?;
    if actual == expected {
        Ok(())
    } else {
        Err(XtaskError::Stale(path.to_owned()))
    }
}

#[allow(clippy::too_many_lines)]
fn contract_fixture() -> ContractFixture {
    let command_meta = CommandMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        client_id: ClientId("client-fixture".to_owned()),
        request_id: RequestId("request-fixture".to_owned()),
    };
    let mut next_sequence = 0;
    let mut event_meta = || {
        let sequence_id = SequenceId(next_sequence);
        next_sequence += 1;
        EventMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            session_id: SessionId("session-fixture".to_owned()),
            sequence_id,
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        }
    };
    let subagent_result = |id: &str, session: &str, text: &str| SubagentResult {
        subagent_id: SubagentId(id.to_owned()),
        session_id: SessionId(session.to_owned()),
        status: SubagentStatus::Completed,
        final_text: text.to_owned(),
        touched_files: Vec::new(),
        diff_artifact: None,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: Cost::Unavailable {
            reason: "fixture".to_owned(),
        },
        turns: 1,
        duration_millis: 5,
    };
    let mixed_output = ToolOutput::Mixed {
        parts: vec![
            ToolOutputPart::Text {
                text: "build output".to_owned(),
            },
            ToolOutputPart::Structured {
                value: json!({"passed": 3, "failed": 0}),
            },
            ToolOutputPart::Image {
                media_type: "image/png".to_owned(),
                data: ImageRef::InlineBase64 {
                    data: "iVBORw0KGgo=".to_owned(),
                },
            },
        ],
    };
    let turn = Turn {
        role: Role::Assistant,
        blocks: vec![
            Block::Text {
                text: "Working".to_owned(),
            },
            Block::Thinking {
                content: "Check the repository".to_owned(),
                signature: Some("opaque-signature".to_owned()),
            },
            Block::ToolCall {
                id: ToolCallId("tool-1".to_owned()),
                name: "bash".to_owned(),
                args: json!({"command": "cargo test"}),
            },
            Block::ToolResult {
                id: ToolCallId("tool-1".to_owned()),
                output: mixed_output.clone(),
                is_error: false,
            },
            Block::Image {
                media_type: "image/png".to_owned(),
                data: ImageRef::Url {
                    url: "https://example.invalid/image.png".to_owned(),
                },
            },
            Block::Citation {
                uri: "https://example.invalid/source".to_owned(),
                title: Some("Source".to_owned()),
                excerpt: None,
            },
        ],
        meta: TurnMeta {
            created_at: Some("2026-01-01T00:00:00Z".to_owned()),
            model: Some("fast".to_owned()),
            synthetic: false,
            summary: false,
        },
    };
    let plan_artifact = PlanArtifact {
        title: "Protocol plan".to_owned(),
        summary_md: "Exercise the durable plan contract.".to_owned(),
        steps: vec![PlanStep {
            description: "Verify generated clients".to_owned(),
            files_touched: vec!["protocol/types.ts".to_owned()],
            verification: "cargo xtask codegen --check".to_owned(),
        }],
        open_questions: Vec::new(),
    };
    let review = SessionReview {
        session_id: SessionId("session-fixture".to_owned()),
        files: vec![SessionReviewFile {
            path: "src/main.rs".to_owned(),
            unified_diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n"
                .to_owned(),
            status: ReviewFileStatus::Pending,
            truncated: false,
            unrestorable_reason: None,
            original_hash: "original-hash".to_owned(),
            current_hash: "current-hash".to_owned(),
        }],
    };
    let session_descriptor = SessionDescriptor {
        session_id: SessionId("session-fork".to_owned()),
        title: "Session fork".to_owned(),
        workspace_name: "workspace".to_owned(),
        model: ModelAlias("fast".to_owned()),
        driver_client_id: Some(ClientId("client-fixture".to_owned())),
        shell_active: false,
    };

    ContractFixture {
        turns: vec![turn],
        client_commands: vec![
            ClientCommand::CreateSession {
                meta: command_meta.clone(),
                cwd: "workspace".to_owned(),
                model: Some(ModelAlias("fast".to_owned())),
            },
            ClientCommand::SendMessage {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                content: "Build it".to_owned(),
                attachments: Vec::new(),
            },
            ClientCommand::AttachSession {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                last_seen_sequence: Some(SequenceId(4)),
                role: ClientRole::Observer,
            },
            ClientCommand::AttachSession {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
            ClientCommand::SendMessage {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                content: "Inspect these".to_owned(),
                attachments: vec![
                    Attachment {
                        name: "notes.txt".to_owned(),
                        source_path: Some("docs/notes.txt".to_owned()),
                        media_type: "text/plain".to_owned(),
                        data: AttachmentData::Text {
                            content: "in-band text".to_owned(),
                        },
                    },
                    Attachment {
                        name: "screen.png".to_owned(),
                        source_path: None,
                        media_type: "image/png".to_owned(),
                        data: AttachmentData::InlineBase64 {
                            data: "iVBORw0KGgo=".to_owned(),
                        },
                    },
                ],
            },
            ClientCommand::ApproveTool {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                invocation_id: ToolInvocationId("tool-1".to_owned()),
                decision: ApprovalDecision::AllowOnce,
                binding: None,
            },
            ClientCommand::ApprovePlan {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                decision: PlanDecision::Approve,
                revisions: None,
            },
            ClientCommand::AnswerQuestion {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                question_id: QuestionId("question-1".to_owned()),
                answers: vec![Answer {
                    question_id: QuestionId("question-1".to_owned()),
                    values: vec!["yes".to_owned()],
                }],
            },
            ClientCommand::Interrupt {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::SwitchMode {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                mode: ModeId("plan".to_owned()),
            },
            ClientCommand::SwitchModel {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                model: ModelAlias("fast".to_owned()),
                provider: None,
            },
            ClientCommand::Compact {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                instructions: None,
            },
            ClientCommand::Fork {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                at_turn: None,
                operation_id: "fork-operation-fixture".to_owned(),
            },
            ClientCommand::Rewind {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                target: RewindTarget::Turn {
                    turn_id: TurnId("turn-fixture".to_owned()),
                },
            },
            ClientCommand::Rewind {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                target: RewindTarget::Source {
                    expected_through: SequenceId(42),
                    source: SequenceId(3),
                    turn_id: TurnId("2".to_owned()),
                    position: RewindSourcePosition::Before,
                },
            },
            ClientCommand::TakeDriver {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::UserShellStarted {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                command: "python".to_owned(),
            },
            ClientCommand::UserShellEnded {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                shell_id: ShellId("shell-fixture".to_owned()),
                status: 0,
                captured_output: None,
            },
            ClientCommand::PinContext {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                item_id: ContextItemId("context-1".to_owned()),
            },
            ClientCommand::EvictContext {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                item_id: ContextItemId("context-2".to_owned()),
            },
            ClientCommand::GetTodos {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::GetContext {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::GetCost {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::GetSessionReview {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
            ClientCommand::ReviewFile {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                path: "src/main.rs".to_owned(),
                decision: ReviewFileDecision::Revert,
                current_hash: "current-hash".to_owned(),
            },
            ClientCommand::SearchSessions {
                meta: command_meta.clone(),
                query: "protocol".to_owned(),
                limit: 25,
            },
            ClientCommand::DumpPrompt {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
                turn_id: Some(TurnId("turn-fixture".to_owned())),
            },
            ClientCommand::ListRuntimeServices {
                meta: command_meta.clone(),
                session_id: SessionId("session-fixture".to_owned()),
            },
        ],
        engine_events: vec![
            EngineEvent::TodosRead {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("todo-ready".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                result: TodoReadResult::Ready {
                    todos: TodoReadSnapshot {
                        through: None,
                        snapshot: TodoSnapshot::default(),
                    },
                },
            },
            EngineEvent::ProviderCallAccounted {
                meta: event_meta(),
                call: ProviderCallIdentity {
                    budget_session_id: SessionId("session-fixture".to_owned()),
                    session_id: SessionId("session-fixture".to_owned()),
                    turn_id: TurnId("turn-fixture".to_owned()),
                    attribution: AccountingAttribution::Main,
                    call_id: "provider-call-fixture".to_owned(),
                    attempt: 0,
                },
                actuals: ProviderCallActuals {
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 20,
                        cache_read_tokens: 80,
                        cache_write_tokens: 0,
                        reasoning_tokens: 5,
                    },
                    cost: Cost::Monetary {
                        amount_micros: 125,
                        currency: "USD".to_owned(),
                    },
                },
            },
            EngineEvent::CommandAcknowledged {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("request-fixture".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: Some(SessionId("session-fixture".to_owned())),
                outcome: CommandOutcome::Accepted {},
            },
            EngineEvent::RuntimeServicesListed {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("runtime-services-fixture".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                services: vec![RuntimeServiceDescriptor {
                    kind: RuntimeServiceKind::Lsp,
                    name: "rust-analyzer".to_owned(),
                }],
            },
            EngineEvent::CommandAcknowledged {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("rejected-request".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: None,
                outcome: CommandOutcome::Rejected {
                    error: EngineError {
                        category: EngineErrorCategory::Protocol,
                        code: "invalid_command".to_owned(),
                        message: "command was rejected".to_owned(),
                        retryable: false,
                        details: None,
                    },
                },
            },
            EngineEvent::SessionCreated {
                meta: event_meta(),
                driver_client_id: ClientId("client-fixture".to_owned()),
            },
            EngineEvent::DriverChanged {
                meta: event_meta(),
                driver_client_id: ClientId("client-fixture".to_owned()),
            },
            EngineEvent::TurnStarted {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
            },
            EngineEvent::TextDelta {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                text: "hello".to_owned(),
            },
            EngineEvent::ThinkingDelta {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                text: "checking".to_owned(),
                signature: None,
            },
            EngineEvent::ToolCallStarted {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                invocation_id: ToolInvocationId("tool-1".to_owned()),
                name: "bash".to_owned(),
                args: json!({"command": "cargo test"}),
                call_index: 0,
            },
            EngineEvent::ToolApprovalNeeded {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                invocation_id: ToolInvocationId("tool-1".to_owned()),
                name: "bash".to_owned(),
                args: json!({"command": "cargo test"}),
                capabilities: vec![ToolCapability::Execute],
                rationale: "runs a local command".to_owned(),
                diff: None,
            },
            EngineEvent::ToolOutputDelta {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                invocation_id: ToolInvocationId("tool-1".to_owned()),
                stream: ToolOutputStream::Stdout,
                chunk: "running tests".to_owned(),
            },
            EngineEvent::ToolCallFinished {
                presentation: None,
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                tool_call_id: ToolCallId("tool-1".to_owned()),
                invocation_id: ToolInvocationId("tool-1".to_owned()),
                output: mixed_output,
                is_error: false,
                call_index: 0,
            },
            EngineEvent::QuestionAsked {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                question_id: QuestionId("question-1".to_owned()),
                questions: vec![
                    Question {
                        id: QuestionId("question-1".to_owned()),
                        prompt: "Continue?".to_owned(),
                        response_kind: QuestionResponseKind::SelectOne,
                        options: vec![QuestionOption {
                            value: "yes".to_owned(),
                            label: "Yes".to_owned(),
                            description: None,
                            model_context_transfer: None,
                        }],
                        model_switch: None,
                    },
                    Question {
                        id: QuestionId("question-model-switch".to_owned()),
                        prompt: "How should the new model receive this conversation?".to_owned(),
                        response_kind: QuestionResponseKind::SelectOne,
                        options: vec![QuestionOption {
                            value: "pass_summary".to_owned(),
                            label: "Pass summary".to_owned(),
                            description: Some(
                                "Compact this conversation, then switch models".to_owned(),
                            ),
                            model_context_transfer: Some(ModelContextTransfer::PassSummary),
                        }],
                        model_switch: Some(ModelSwitchQuestion {
                            model: ModelAlias("openai/gpt-5".to_owned()),
                            provider: Some("openai".to_owned()),
                        }),
                    },
                ],
            },
            EngineEvent::QuestionAnswered {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                question_id: QuestionId("question-1".to_owned()),
                answers: vec![Answer {
                    question_id: QuestionId("question-1".to_owned()),
                    values: vec!["yes".to_owned()],
                }],
            },
            EngineEvent::TurnFinished {
                meta: event_meta(),
                turn_id: TurnId("turn-fixture".to_owned()),
                status: TurnStatus::Completed,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: 80,
                    cache_write_tokens: 0,
                    reasoning_tokens: 5,
                },
                cost: Cost::Monetary {
                    amount_micros: 125,
                    currency: "USD".to_owned(),
                },
            },
            EngineEvent::CompactionStarted {
                meta: event_meta(),
                reason: CompactionReason::Automatic,
            },
            EngineEvent::CompactionAttemptStarted {
                session_id: SessionId("session-fixture".to_owned()),
                summary_turn_id: TurnId("summary-turn".to_owned()),
                attempt: 0,
            },
            EngineEvent::CompactionThinkingDelta {
                session_id: SessionId("session-fixture".to_owned()),
                summary_turn_id: TurnId("summary-turn".to_owned()),
                attempt: 0,
                text: "Identifying durable context".to_owned(),
            },
            EngineEvent::CompactionTextDelta {
                session_id: SessionId("session-fixture".to_owned()),
                summary_turn_id: TurnId("summary-turn".to_owned()),
                attempt: 0,
                text: "## Goal\nContinue the task.".to_owned(),
            },
            EngineEvent::CompactionFinished {
                meta: event_meta(),
                summary_turn_id: TurnId("summary-turn".to_owned()),
                reclaimed_tokens: 25_000,
                usage: None,
                cost: None,
            },
            EngineEvent::CompactionFailed {
                meta: event_meta(),
                summary_turn_id: TurnId("failed-summary-turn".to_owned()),
            },
            EngineEvent::SubagentSpawned {
                meta: event_meta(),
                subagent_id: SubagentId("subagent-1".to_owned()),
                child_session_id: SessionId("child-session-1".to_owned()),
                task: "inspect protocol".to_owned(),
            },
            EngineEvent::SubagentFinished {
                meta: event_meta(),
                subagent_id: SubagentId("subagent-1".to_owned()),
                result: subagent_result("subagent-1", "child-session-1", "done"),
            },
            EngineEvent::SubagentFinished {
                meta: event_meta(),
                subagent_id: SubagentId("subagent-2".to_owned()),
                result: subagent_result("subagent-2", "child-session-2", "three files"),
            },
            EngineEvent::ToolOutputPruned {
                meta: event_meta(),
                tool_call_id: ToolCallId("tool-old".to_owned()),
                reclaimed_tokens: 21_000,
            },
            EngineEvent::ModeChanged {
                meta: event_meta(),
                mode: ModeId("plan".to_owned()),
                definition_fingerprint: "fixture".to_owned(),
            },
            EngineEvent::ModelChanged {
                meta: event_meta(),
                model: ModelAlias("fast".to_owned()),
                provider: None,
                thinking: Some(ThinkingLevel::Off),
            },
            EngineEvent::ModelContextCleared {
                meta: event_meta(),
                strategy: ModelContextTransfer::StartWithoutContext,
            },
            EngineEvent::ContextItemPinned {
                meta: event_meta(),
                item_id: ContextItemId("context-1".to_owned()),
                effective_after_agent_turn: 3,
            },
            EngineEvent::ContextItemEvicted {
                meta: event_meta(),
                item_id: ContextItemId("context-2".to_owned()),
                effective_after_agent_turn: 3,
            },
            EngineEvent::UserShellStateChanged {
                meta: event_meta(),
                shell_id: ShellId("shell-fixture".to_owned()),
                command: Some("python".to_owned()),
                active: false,
                status: None,
                captured_output: None,
            },
            EngineEvent::Error {
                meta: event_meta(),
                error: EngineError {
                    category: EngineErrorCategory::Protocol,
                    code: "invalid_command".to_owned(),
                    message: "command was rejected".to_owned(),
                    retryable: false,
                    details: Some(json!({"field": "type"})),
                },
            },
            EngineEvent::PlanSubmitted {
                meta: event_meta(),
                artifact: plan_artifact.clone(),
            },
            EngineEvent::PlanReviewed {
                meta: event_meta(),
                artifact: plan_artifact,
                decision: PlanDecision::Approve,
                revisions: None,
            },
            EngineEvent::SessionReviewReady {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("review-ready".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                review: review.clone(),
            },
            EngineEvent::SessionReviewUpdated {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("review-updated".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                path: "src/main.rs".to_owned(),
                decision: ReviewFileDecision::Revert,
                review,
            },
            EngineEvent::SessionReplayCompleted {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("replay-complete".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                session_id: SessionId("session-fixture".to_owned()),
                through_sequence: Some(SequenceId(27)),
            },
            EngineEvent::SessionForked {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("fork-complete".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                parent_session_id: SessionId("session-fixture".to_owned()),
                child: session_descriptor.clone(),
                at_turn: TurnId("turn-fixture".to_owned()),
            },
            EngineEvent::SessionsSearchReady {
                meta: CommandAckMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: ClientId("client-fixture".to_owned()),
                    request_id: RequestId("search-complete".to_owned()),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                },
                query: "protocol".to_owned(),
                sessions: vec![session_descriptor],
                truncated: false,
            },
        ],
    }
}
