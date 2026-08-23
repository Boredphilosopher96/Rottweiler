//! Dependency-leaf owner of the public Rottweiler plugin wire protocol.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

pub const JSON_RPC_VERSION: &str = "2.0";
pub const PLUGIN_HOST_ID: &str = "rottweiler";
pub const PROTOCOL_VERSION: u32 = 2;
pub const MIN_PROTOCOL_VERSION: u32 = PROTOCOL_VERSION;
pub const SUPPORTED_PROTOCOL_VERSIONS: [u32; 1] = [PROTOCOL_VERSION];
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
pub const MAX_CAPABILITIES_PER_KIND: usize = 256;
pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_VERSION_BYTES: usize = 64;
pub const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;
pub const MAX_SCHEMA_BYTES: usize = 64 * 1024;
pub const MAX_RPC_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_HOOK_PAYLOAD_BYTES: usize = 256 * 1024;
pub const MAX_SCHEMA_DEPTH: usize = 32;
pub const MAX_PLUGIN_MODEL_TOKENS: u64 = 16 * 1024 * 1024;
pub const MAX_PLUGIN_PRICE_MICROS_USD: u64 = 1_000_000_000_000;
pub const DEFAULT_HANDLER_TIMEOUT_MS: u64 = 5_000;

/// Maximum wire length of a provider alias prefix, including its trailing slash.
pub const MAX_PROVIDER_ALIAS_PREFIX_BYTES: usize = MAX_NAME_BYTES;

/// A provider alias prefix did not satisfy the public plugin contract.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "provider alias prefix must be 2-{MAX_PROVIDER_ALIAS_PREFIX_BYTES} ASCII bytes, end in '/', and contain only lowercase letters, digits, '-', '_', or '.' before '/'"
)]
pub struct ProviderAliasPrefixError;

/// Validates the canonical alias prefix shared by manifests and runtime composition.
///
/// # Errors
///
/// Returns an error when the prefix is not a bounded canonical wire value.
pub fn validate_provider_alias_prefix(prefix: &str) -> Result<(), ProviderAliasPrefixError> {
    let stem = prefix
        .strip_suffix('/')
        .filter(|stem| !stem.is_empty())
        .ok_or(ProviderAliasPrefixError)?;
    if prefix.len() > MAX_PROVIDER_ALIAS_PREFIX_BYTES
        || !stem.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(ProviderAliasPrefixError);
    }
    Ok(())
}

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_TOOL_CALL: &str = "tool/call";
pub const METHOD_COMMAND_EXECUTE: &str = "command/execute";
pub const METHOD_HOOK_INVOKE: &str = "hook/invoke";
pub const METHOD_PROVIDER_COMPLETE: &str = "provider/complete";
pub const METHOD_PROVIDER_MODELS: &str = "provider/models";
pub const METHOD_PROVIDER_EVENT: &str = "provider/event";
pub const METHOD_PROVIDER_CANCEL: &str = "provider/cancel";
pub const METHOD_PROVIDER_HTTP: &str = "provider/http";
pub const METHOD_PROVIDER_HTTP_EVENT: &str = "provider/http_event";
pub const METHOD_PROVIDER_HTTP_CANCEL: &str = "provider/http_cancel";
pub const METHOD_EVENT_PUBLISH: &str = "event/publish";
pub const METHOD_SESSION_INJECT_MESSAGE: &str = "session/inject_message";
pub const METHOD_SESSION_SET_STATUS: &str = "session/set_status";
pub const METHOD_UI_NOTIFY: &str = "ui/notify";
pub const METHOD_SHUTDOWN: &str = "shutdown";
pub const METHOD_EXIT: &str = "exit";

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: RpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcSuccess {
    pub jsonrpc: String,
    pub id: Option<RpcId>,
    pub result: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcFailure {
    pub jsonrpc: String,
    pub id: Option<RpcId>,
    pub error: RpcErrorObject,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RpcFrame {
    Request(RpcRequest),
    Notification(RpcNotification),
    Success(RpcSuccess),
    Failure(RpcFailure),
}

impl RpcFrame {
    fn validate(&self) -> Result<(), FrameError> {
        let (version, method, id) = match self {
            Self::Request(frame) => (&frame.jsonrpc, Some(frame.method.as_str()), Some(&frame.id)),
            Self::Notification(frame) => (&frame.jsonrpc, Some(frame.method.as_str()), None),
            Self::Success(frame) => (&frame.jsonrpc, None, frame.id.as_ref()),
            Self::Failure(frame) => (&frame.jsonrpc, None, frame.id.as_ref()),
        };
        if version != JSON_RPC_VERSION {
            return Err(FrameError::InvalidVersion);
        }
        if method.is_some_and(|method| method.is_empty() || method.len() > MAX_NAME_BYTES) {
            return Err(FrameError::InvalidMethod);
        }
        if id.is_some_and(|id| matches!(id, RpcId::String(value) if value.is_empty() || value.len() > MAX_NAME_BYTES || value.chars().any(char::is_control))) {
            return Err(FrameError::InvalidId);
        }
        if let Self::Failure(frame) = self
            && (frame.error.message.is_empty()
                || frame.error.message.len() > MAX_RPC_MESSAGE_BYTES
                || frame.error.message.chars().any(char::is_control))
        {
            return Err(FrameError::InvalidMessage);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("plugin frame exceeds the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error("plugin frame is empty")]
    Empty,
    #[error("plugin frame contains malformed JSON: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("plugin frame must use JSON-RPC 2.0")]
    InvalidVersion,
    #[error("plugin RPC method is empty or too long")]
    InvalidMethod,
    #[error("plugin RPC ID string is empty, too long, or contains control characters")]
    InvalidId,
    #[error("plugin RPC error message is empty, too long, or contains control characters")]
    InvalidMessage,
}

/// Incremental newline-delimited JSON-RPC decoder with a hard memory bound.
#[derive(Debug)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    max_frame_bytes: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(MAX_FRAME_BYTES)
    }
}

impl FrameDecoder {
    #[must_use]
    pub const fn new(max_frame_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_frame_bytes: if max_frame_bytes < MAX_FRAME_BYTES {
                max_frame_bytes
            } else {
                MAX_FRAME_BYTES
            },
        }
    }

    /// Consumes input and returns every complete frame. Partial frames remain buffered.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized, empty, malformed, or invalid JSON-RPC frame.
    pub fn push(&mut self, input: &[u8]) -> Result<Vec<RpcFrame>, FrameError> {
        let mut frames = Vec::new();
        let mut remaining = input;
        while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
            let part = &remaining[..newline];
            if self.buffer.len().saturating_add(part.len()) > self.max_frame_bytes {
                self.buffer.clear();
                return Err(FrameError::TooLarge {
                    limit: self.max_frame_bytes,
                });
            }
            self.buffer.extend_from_slice(part);
            if self.buffer.last() == Some(&b'\r') {
                self.buffer.pop();
            }
            if self.buffer.is_empty() {
                return Err(FrameError::Empty);
            }
            let complete = std::mem::take(&mut self.buffer);
            let frame: RpcFrame = serde_json::from_slice(&complete)?;
            frame.validate()?;
            frames.push(frame);
            remaining = &remaining[newline + 1..];
        }
        if self.buffer.len().saturating_add(remaining.len()) > self.max_frame_bytes {
            self.buffer.clear();
            return Err(FrameError::TooLarge {
                limit: self.max_frame_bytes,
            });
        }
        self.buffer.extend_from_slice(remaining);
        Ok(frames)
    }

    #[must_use]
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

/// Encodes one validated newline-delimited JSON-RPC frame.
///
/// # Errors
///
/// Returns an error when the frame is invalid, cannot be serialized, or exceeds the limit.
pub fn encode_frame(frame: &RpcFrame, max_frame_bytes: usize) -> Result<Vec<u8>, FrameError> {
    frame.validate()?;
    let max_frame_bytes = max_frame_bytes.min(MAX_FRAME_BYTES);
    let mut encoded = serde_json::to_vec(frame)?;
    if encoded.len() > max_frame_bytes {
        return Err(FrameError::TooLarge {
            limit: max_frame_bytes,
        });
    }
    encoded.push(b'\n');
    Ok(encoded)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub protocol: u32,
    pub capabilities: PluginCapabilities,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCapabilities {
    #[serde(default)]
    pub tools: Vec<PluginToolCapability>,
    #[serde(default)]
    pub commands: Vec<PluginCommandCapability>,
    #[serde(default)]
    pub hooks: Vec<PluginHookDeclaration>,
    #[serde(default)]
    pub providers: Vec<PluginProviderCapability>,
    #[serde(default)]
    pub event_subscriptions: Vec<String>,
    #[serde(default, alias = "push_methods")]
    pub push: Vec<PluginPush>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolCapability {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub caps: Vec<PluginToolEffect>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum PluginToolEffect {
    #[serde(rename = "reads-fs")]
    ReadsFilesystem,
    #[serde(rename = "writes-fs")]
    WritesFilesystem,
    #[serde(rename = "network")]
    Network,
    #[serde(rename = "exec")]
    Execute,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCommandCapability {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginHook {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreTool,
    PostTool,
    PreCompact,
    TurnEnd,
    PermissionCheck,
}

impl PluginHook {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
            Self::PreCompact => "pre_compact",
            Self::TurnEnd => "turn_end",
            Self::PermissionCheck => "permission_check",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginHookFailurePolicy {
    FailOpen,
    FailClosed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged)]
pub enum PluginHookDeclaration {
    Name(PluginHook),
    Detailed(PluginHookCapability),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHookCapability {
    pub name: PluginHook,
    pub failure_policy: PluginHookFailurePolicy,
}

impl PluginHookDeclaration {
    #[must_use]
    pub const fn name(self) -> PluginHook {
        match self {
            Self::Name(name) => name,
            Self::Detailed(declaration) => declaration.name,
        }
    }

    #[must_use]
    pub const fn failure_policy(self) -> PluginHookFailurePolicy {
        match self {
            Self::Name(_) => PluginHookFailurePolicy::FailOpen,
            Self::Detailed(declaration) => declaration.failure_policy,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginProviderCapability {
    #[serde(rename = "alias-prefix")]
    pub alias_prefix: String,
    /// Approval-fingerprinted protocol capabilities for this provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Configured credential references this provider may name in host HTTP calls.
    /// Values, unlike these identifiers, never cross the plugin boundary.
    #[serde(
        default,
        rename = "credential-references",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub credential_references: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum PluginPush {
    #[serde(rename = "session/inject_message")]
    SessionInjectMessage,
    #[serde(rename = "session/set_status")]
    SessionSetStatus,
    #[serde(rename = "ui/notify")]
    UiNotify,
}

impl PluginPush {
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::SessionInjectMessage => METHOD_SESSION_INJECT_MESSAGE,
            Self::SessionSetStatus => METHOD_SESSION_SET_STATUS,
            Self::UiNotify => METHOD_UI_NOTIFY,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ManifestError {
    #[error("manifest exceeds the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error("manifest JSON is invalid: {message}")]
    Malformed { message: String },
    #[error("protocol {protocol} is unsupported; expected {minimum}..={maximum}")]
    UnsupportedProtocol {
        protocol: u32,
        minimum: u32,
        maximum: u32,
    },
    #[error("manifest field `{field}` is invalid: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("manifest capability `{kind}` exceeds the {limit}-entry limit")]
    TooManyCapabilities { kind: &'static str, limit: usize },
    #[error("manifest contains duplicate {kind} `{name}`")]
    Duplicate { kind: &'static str, name: String },
}

impl PluginManifest {
    /// Parses and validates a bounded manifest.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized, malformed, unsupported, or invalid manifests.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                limit: MAX_MANIFEST_BYTES,
            });
        }
        let manifest: Self =
            serde_json::from_slice(bytes).map_err(|error| ManifestError::Malformed {
                message: error.to_string(),
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validates protocol compatibility and every declared capability.
    ///
    /// # Errors
    ///
    /// Returns an error for any invalid or out-of-bounds manifest field.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&self.protocol) {
            return Err(ManifestError::UnsupportedProtocol {
                protocol: self.protocol,
                minimum: MIN_PROTOCOL_VERSION,
                maximum: PROTOCOL_VERSION,
            });
        }
        validate_name(&self.name, NameKind::Plugin, "name")?;
        validate_text(&self.version, 1, MAX_VERSION_BYTES, "version")?;
        self.capabilities.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|error| ManifestError::Malformed {
            message: error.to_string(),
        })?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                limit: MAX_MANIFEST_BYTES,
            });
        }
        Ok(())
    }

    /// Computes the BLAKE3 digest of the canonical manifest representation.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest is invalid or cannot be serialized.
    pub fn fingerprint(&self) -> Result<String, ManifestError> {
        self.validate()?;
        let mut value = serde_json::to_value(self).map_err(|error| ManifestError::Malformed {
            message: error.to_string(),
        })?;
        normalize_capability_arrays(&mut value);
        let canonical = canonicalize(value);
        let bytes = serde_json::to_vec(&canonical).map_err(|error| ManifestError::Malformed {
            message: error.to_string(),
        })?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    /// Alias for [`Self::fingerprint`] that makes the normalization contract explicit.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest is invalid or cannot be serialized.
    pub fn canonical_fingerprint(&self) -> Result<String, ManifestError> {
        self.fingerprint()
    }
}

impl PluginCapabilities {
    fn validate(&self) -> Result<(), ManifestError> {
        validate_count("tools", self.tools.len())?;
        validate_count("commands", self.commands.len())?;
        validate_count("hooks", self.hooks.len())?;
        validate_count("providers", self.providers.len())?;
        validate_count("event_subscriptions", self.event_subscriptions.len())?;
        validate_count("push", self.push.len())?;

        let mut names = BTreeSet::new();
        for tool in &self.tools {
            validate_name(&tool.name, NameKind::Tool, "tools.name")?;
            validate_text(
                &tool.description,
                1,
                MAX_DESCRIPTION_BYTES,
                "tools.description",
            )?;
            validate_schema(&tool.schema)?;
            if tool.caps.len() > MAX_CAPABILITIES_PER_KIND {
                return Err(ManifestError::TooManyCapabilities {
                    kind: "tool caps",
                    limit: MAX_CAPABILITIES_PER_KIND,
                });
            }
            let mut caps = BTreeSet::new();
            for cap in &tool.caps {
                if !caps.insert(cap) {
                    return Err(ManifestError::Duplicate {
                        kind: "tool capability",
                        name: format!("{cap:?}"),
                    });
                }
            }
            if !names.insert(tool.name.as_str()) {
                return Err(ManifestError::Duplicate {
                    kind: "tool",
                    name: tool.name.clone(),
                });
            }
        }
        validate_unique_named(
            "command",
            self.commands.iter().map(|command| command.name.as_str()),
            NameKind::Command,
        )?;
        for command in &self.commands {
            validate_text(
                &command.description,
                1,
                MAX_DESCRIPTION_BYTES,
                "commands.description",
            )?;
            if let Some(argument_hint) = &command.argument_hint {
                validate_text(
                    argument_hint,
                    1,
                    MAX_DESCRIPTION_BYTES,
                    "commands.argument_hint",
                )?;
            }
            validate_count("command allowed_tools", command.allowed_tools.len())?;
            validate_unique_named(
                "command allowed tool",
                command.allowed_tools.iter().map(String::as_str),
                NameKind::Tool,
            )?;
        }
        validate_unique("hook", self.hooks.iter().map(|hook| hook.name().as_str()))?;

        let mut prefixes = BTreeSet::new();
        for provider in &self.providers {
            validate_provider_prefix(&provider.alias_prefix)?;
            validate_count("provider capabilities", provider.capabilities.len())?;
            validate_unique_named(
                "provider capability",
                provider.capabilities.iter().map(String::as_str),
                NameKind::Command,
            )?;
            validate_count(
                "provider credential references",
                provider.credential_references.len(),
            )?;
            validate_unique_named(
                "provider credential reference",
                provider.credential_references.iter().map(String::as_str),
                NameKind::Command,
            )?;
            if !prefixes.insert(provider.alias_prefix.as_str()) {
                return Err(ManifestError::Duplicate {
                    kind: "provider prefix",
                    name: provider.alias_prefix.clone(),
                });
            }
        }
        validate_unique_named(
            "event subscription",
            self.event_subscriptions.iter().map(String::as_str),
            NameKind::Event,
        )?;
        validate_unique("push method", self.push.iter().map(|push| push.method()))?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum NameKind {
    Plugin,
    Tool,
    Command,
    Event,
}

fn validate_name(name: &str, kind: NameKind, field: &'static str) -> Result<(), ManifestError> {
    let valid_bytes = match kind {
        NameKind::Plugin | NameKind::Command => name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        }),
        NameKind::Tool => name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
        NameKind::Event => {
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && name
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_uppercase())
        }
    };
    if name.is_empty() || name.len() > MAX_NAME_BYTES || !valid_bytes {
        return Err(ManifestError::InvalidField {
            field,
            reason: "must be a bounded canonical name",
        });
    }
    Ok(())
}

fn validate_text(
    text: &str,
    minimum: usize,
    maximum: usize,
    field: &'static str,
) -> Result<(), ManifestError> {
    if text.len() < minimum || text.len() > maximum || text.chars().any(char::is_control) {
        return Err(ManifestError::InvalidField {
            field,
            reason: "has an invalid length or contains control characters",
        });
    }
    Ok(())
}

fn validate_count(kind: &'static str, count: usize) -> Result<(), ManifestError> {
    if count > MAX_CAPABILITIES_PER_KIND {
        Err(ManifestError::TooManyCapabilities {
            kind,
            limit: MAX_CAPABILITIES_PER_KIND,
        })
    } else {
        Ok(())
    }
}

fn validate_unique<'a>(
    kind: &'static str,
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), ManifestError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(ManifestError::Duplicate {
                kind,
                name: value.to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_unique_named<'a>(
    kind: &'static str,
    values: impl IntoIterator<Item = &'a str>,
    name_kind: NameKind,
) -> Result<(), ManifestError> {
    let values: Vec<_> = values.into_iter().collect();
    for value in &values {
        validate_name(value, name_kind, "capability name")?;
    }
    validate_unique(kind, values)
}

fn validate_provider_prefix(prefix: &str) -> Result<(), ManifestError> {
    validate_provider_alias_prefix(prefix).map_err(|_| ManifestError::InvalidField {
        field: "providers.alias-prefix",
        reason: "must be a bounded canonical prefix ending in `/`",
    })
}

fn validate_schema(schema: &Value) -> Result<(), ManifestError> {
    if !schema.is_object() {
        return Err(ManifestError::InvalidField {
            field: "tools.schema",
            reason: "must be a JSON Schema object",
        });
    }
    let bytes = serde_json::to_vec(schema).map_err(|error| ManifestError::Malformed {
        message: error.to_string(),
    })?;
    if bytes.len() > MAX_SCHEMA_BYTES || json_depth(schema, 0) > MAX_SCHEMA_DEPTH {
        return Err(ManifestError::InvalidField {
            field: "tools.schema",
            reason: "exceeds the schema size or nesting limit",
        });
    }
    if let Some(schema_type) = schema.get("type")
        && !matches!(schema_type, Value::String(_) | Value::Array(_))
    {
        return Err(ManifestError::InvalidField {
            field: "tools.schema.type",
            reason: "must be a string or array",
        });
    }
    Ok(())
}

fn json_depth(value: &Value, depth: usize) -> usize {
    match value {
        Value::Array(values) => values
            .iter()
            .map(|value| json_depth(value, depth + 1))
            .max()
            .unwrap_or(depth),
        Value::Object(values) => values
            .values()
            .map(|value| json_depth(value, depth + 1))
            .max()
            .unwrap_or(depth),
        _ => depth,
    }
}

fn normalize_capability_arrays(value: &mut Value) {
    let Some(capabilities) = value.get_mut("capabilities").and_then(Value::as_object_mut) else {
        return;
    };
    for key in [
        "tools",
        "commands",
        "hooks",
        "providers",
        "event_subscriptions",
        "push",
    ] {
        if let Some(values) = capabilities.get_mut(key).and_then(Value::as_array_mut) {
            if key == "tools" {
                for tool in values.iter_mut() {
                    if let Some(caps) = tool.get_mut("caps").and_then(Value::as_array_mut) {
                        caps.sort_by_key(Value::to_string);
                    }
                }
            }
            if key == "commands" {
                for command in values.iter_mut() {
                    if let Some(tools) = command
                        .get_mut("allowed_tools")
                        .and_then(Value::as_array_mut)
                    {
                        tools.sort_by_key(Value::to_string);
                    }
                }
            }
            if key == "providers" {
                for provider in values.iter_mut() {
                    if let Some(provider_capabilities) = provider
                        .get_mut("capabilities")
                        .and_then(Value::as_array_mut)
                    {
                        provider_capabilities.sort_by_key(Value::to_string);
                    }
                    if let Some(credential_references) = provider
                        .get_mut("credential-references")
                        .and_then(Value::as_array_mut)
                    {
                        credential_references.sort_by_key(Value::to_string);
                    }
                }
            }
            values.sort_by_key(Value::to_string);
        }
    }
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect::<Map<_, _>>(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    pub host: String,
    pub protocol: u32,
    pub min_protocol: u32,
    pub max_frame_bytes: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallParams {
    pub name: String,
    pub input: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandExecuteParams {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HookInvokeParams {
    pub hook: PluginHook,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCompleteParams {
    pub alias: String,
    pub request: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEventParams {
    pub request_id: RpcId,
    pub event: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventPublishParams {
    pub event: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelsParams {
    pub alias_prefix: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheBreakpoints {
    None,
    Explicit,
    Automatic,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelCapabilities {
    pub tool_calling: bool,
    pub vision: bool,
    pub thinking: bool,
    pub cache_breakpoints: ProviderCacheBreakpoints,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelPricing {
    pub input_per_million_micros_usd: u64,
    pub output_per_million_micros_usd: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_million_micros_usd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million_micros_usd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_per_million_micros_usd: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub capabilities: ProviderModelCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ProviderModelPricing>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelsResponse {
    pub models: Vec<ProviderModel>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<ProviderHttpHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
    pub credential_header: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_prefix: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHttpCapabilityParams {
    pub alias: String,
    pub credential_reference: String,
    pub request: ProviderHttpRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCancelParams {
    pub request_id: RpcId,
}
