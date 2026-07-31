//! Bounded JSON-RPC plugin protocol, manifest validation, and capability guards.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{HookDirective, HookError, HookEvent, HookHandler, HookInvocation};

pub const JSON_RPC_VERSION: &str = "2.0";
pub const PROTOCOL_VERSION: u32 = 1;
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

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_TOOL_CALL: &str = "tool/call";
pub const METHOD_COMMAND_EXECUTE: &str = "command/execute";
pub const METHOD_HOOK_INVOKE: &str = "hook/invoke";
pub const METHOD_PROVIDER_COMPLETE: &str = "provider/complete";
pub const METHOD_PROVIDER_EVENT: &str = "provider/event";
pub const METHOD_PROVIDER_CANCEL: &str = "provider/cancel";
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

impl From<PluginHookFailurePolicy> for crate::HookFailurePolicy {
    fn from(policy: PluginHookFailurePolicy) -> Self {
        match policy {
            PluginHookFailurePolicy::FailOpen => Self::FailOpen,
            PluginHookFailurePolicy::FailClosed => Self::FailClosed,
        }
    }
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

    #[must_use]
    pub fn registration(self, id: impl Into<String>) -> crate::HookRegistration {
        crate::HookRegistration::new(id, self.name().into())
            .with_failure_policy(self.failure_policy().into())
    }
}

impl From<PluginHook> for HookEvent {
    fn from(hook: PluginHook) -> Self {
        match hook {
            PluginHook::SessionStart => Self::SessionStart,
            PluginHook::SessionEnd => Self::SessionEnd,
            PluginHook::UserPromptSubmit => Self::UserPromptSubmit,
            PluginHook::PreTool => Self::PreTool,
            PluginHook::PostTool => Self::PostTool,
            PluginHook::PreCompact => Self::PreCompact,
            PluginHook::TurnEnd => Self::TurnEnd,
            PluginHook::PermissionCheck => Self::PermissionCheck,
        }
    }
}

impl From<HookEvent> for PluginHook {
    fn from(event: HookEvent) -> Self {
        match event {
            HookEvent::SessionStart => Self::SessionStart,
            HookEvent::SessionEnd => Self::SessionEnd,
            HookEvent::UserPromptSubmit => Self::UserPromptSubmit,
            HookEvent::PreTool => Self::PreTool,
            HookEvent::PostTool => Self::PostTool,
            HookEvent::PreCompact => Self::PreCompact,
            HookEvent::TurnEnd => Self::TurnEnd,
            HookEvent::PermissionCheck => Self::PermissionCheck,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginProviderCapability {
    #[serde(rename = "alias-prefix")]
    pub alias_prefix: String,
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
    if prefix.len() < 2
        || prefix.len() > MAX_NAME_BYTES
        || !prefix.ends_with('/')
        || !prefix[..prefix.len() - 1].bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(ManifestError::InvalidField {
            field: "providers.alias-prefix",
            reason: "must be a bounded canonical prefix ending in `/`",
        });
    }
    Ok(())
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
    if bytes.len() > MAX_SCHEMA_BYTES || json_depth(schema, 0) > 32 {
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

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("approval store failed: {message}")]
pub struct ApprovalStoreError {
    pub message: String,
}

pub trait ApprovalStore: Send + Sync {
    /// Loads the last explicitly approved fingerprint, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing store cannot be read.
    fn approved_fingerprint(&self, plugin_name: &str)
    -> Result<Option<String>, ApprovalStoreError>;
    /// Persists one explicit manifest approval.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing store cannot be written durably.
    fn record_approval(
        &self,
        plugin_name: &str,
        fingerprint: &str,
    ) -> Result<(), ApprovalStoreError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalRequirement {
    Approved,
    FirstLoad { fingerprint: String },
    ManifestChanged { previous: String, current: String },
}

/// Compares a manifest with the last explicit approval.
///
/// # Errors
///
/// Returns an error when validation, fingerprinting, or store access fails.
pub fn approval_requirement(
    store: &dyn ApprovalStore,
    manifest: &PluginManifest,
) -> Result<ApprovalRequirement, PluginApprovalError> {
    let current = manifest.fingerprint()?;
    match store.approved_fingerprint(&manifest.name)? {
        None => Ok(ApprovalRequirement::FirstLoad {
            fingerprint: current,
        }),
        Some(previous) if previous == current => Ok(ApprovalRequirement::Approved),
        Some(previous) => Ok(ApprovalRequirement::ManifestChanged { previous, current }),
    }
}

/// Records explicit approval for the manifest's current fingerprint.
///
/// # Errors
///
/// Returns an error when validation, fingerprinting, or persistence fails.
pub fn approve_manifest(
    store: &dyn ApprovalStore,
    manifest: &PluginManifest,
) -> Result<String, PluginApprovalError> {
    let fingerprint = manifest.fingerprint()?;
    store.record_approval(&manifest.name, &fingerprint)?;
    Ok(fingerprint)
}

#[derive(Debug, Error)]
pub enum PluginApprovalError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Store(#[from] ApprovalStoreError),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PluginProcessConfigError {
    #[error("plugin executable must resolve from an absolute path to a real executable file")]
    InvalidExecutable,
    #[error("plugin working directory is invalid: {0}")]
    InvalidCwd(String),
    #[error("plugin environment allowlist contains an invalid variable name")]
    InvalidEnvironmentName,
    #[error("plugin environment variable is not in the safe host allowlist")]
    UnsafeEnvironmentName,
    #[error("plugin argv contains an interior NUL byte")]
    InvalidArgument,
    #[error("plugin network allowlist contains an invalid public domain")]
    InvalidAllowedDomain,
    #[error("plugin content attestation contains an invalid regular file")]
    InvalidAttestedFile,
    #[error("plugin content attestation exceeds its file or byte limit")]
    AttestationLimit,
}

/// A direct-exec process description. Launchers must clear the environment and
/// restore only the named variables; no field is interpreted by a shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginProcessConfig {
    executable: PathBuf,
    argv: Vec<OsString>,
    cwd: PathBuf,
    environment_allowlist: BTreeSet<OsString>,
    allowed_domains: BTreeSet<String>,
    executable_identity: ExecutableIdentity,
    attested_files: Vec<ExecutableIdentity>,
    code_root: Option<CodeRootIdentity>,
}

/// Stable filesystem identity pinned when a plugin executable is configured.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExecutableIdentity {
    pub canonical_path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub length: u64,
    pub content_blake3: String,
}

/// Stable identity for the narrowly readable plugin package directory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CodeRootIdentity {
    pub canonical_path: PathBuf,
    pub device: u64,
    pub inode: u64,
}

impl PluginProcessConfig {
    /// Creates a direct-exec configuration using the validated current directory.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid executable or current directory.
    pub fn new(executable: impl Into<PathBuf>) -> Result<Self, PluginProcessConfigError> {
        let executable = validate_executable(&executable.into())?;
        let cwd = std::env::current_dir()
            .map_err(|error| PluginProcessConfigError::InvalidCwd(error.to_string()))?;
        let cwd = validate_cwd(&cwd)?;
        Ok(Self {
            executable_identity: executable_identity(&executable)?,
            executable,
            argv: Vec::new(),
            cwd,
            environment_allowlist: BTreeSet::new(),
            allowed_domains: BTreeSet::new(),
            attested_files: Vec::new(),
            code_root: None,
        })
    }

    /// Replaces the literal argument vector.
    ///
    /// # Errors
    ///
    /// Returns an error when an argument contains an interior NUL byte.
    pub fn with_argv(
        mut self,
        argv: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Result<Self, PluginProcessConfigError> {
        self.argv = argv.into_iter().map(Into::into).collect();
        if self.argv.iter().any(|arg| os_contains_nul(arg)) {
            return Err(PluginProcessConfigError::InvalidArgument);
        }
        Ok(self)
    }

    /// Sets and canonicalizes the plugin working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the path does not resolve to a directory.
    pub fn with_cwd(mut self, cwd: impl AsRef<Path>) -> Result<Self, PluginProcessConfigError> {
        self.cwd = validate_cwd(cwd.as_ref())?;
        Ok(self)
    }

    /// Sets the environment variable names restored after clearing the environment.
    ///
    /// # Errors
    ///
    /// Returns an error when a variable name is not canonical uppercase ASCII.
    pub fn with_environment_allowlist(
        mut self,
        names: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Result<Self, PluginProcessConfigError> {
        self.environment_allowlist = names.into_iter().map(Into::into).collect();
        if self
            .environment_allowlist
            .iter()
            .any(|name| !valid_environment_name(name))
        {
            return Err(PluginProcessConfigError::InvalidEnvironmentName);
        }
        if self
            .environment_allowlist
            .iter()
            .any(|name| !safe_environment_name(name))
        {
            return Err(PluginProcessConfigError::UnsafeEnvironmentName);
        }
        Ok(self)
    }

    /// Sets the exact public DNS names approved for plugin network egress.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, local, or excessive domain entries.
    pub fn with_allowed_domains(
        mut self,
        domains: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, PluginProcessConfigError> {
        self.allowed_domains = domains.into_iter().map(Into::into).collect();
        if self.allowed_domains.len() > MAX_CAPABILITIES_PER_KIND
            || self
                .allowed_domains
                .iter()
                .any(|domain| !valid_public_domain(domain))
        {
            return Err(PluginProcessConfigError::InvalidAllowedDomain);
        }
        Ok(self)
    }

    /// Pins interpreter entrypoints and adjacent dependency descriptors whose
    /// contents affect the approved plugin process.
    ///
    /// # Errors
    ///
    /// Returns an error for non-regular files, duplicates, or excessive
    /// attestation work.
    pub fn with_attested_files(
        mut self,
        paths: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Result<Self, PluginProcessConfigError> {
        const MAX_ATTESTED_FILES: usize = 64;
        const MAX_ATTESTED_BYTES: u64 = 256 * 1024 * 1024;
        let mut canonical = paths
            .into_iter()
            .map(Into::into)
            .map(|path| {
                if std::fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(PluginProcessConfigError::InvalidAttestedFile);
                }
                let path = std::fs::canonicalize(path)
                    .map_err(|_| PluginProcessConfigError::InvalidAttestedFile)?;
                if !path.is_file() {
                    return Err(PluginProcessConfigError::InvalidAttestedFile);
                }
                Ok(path)
            })
            .collect::<Result<Vec<_>, _>>()?;
        canonical.sort();
        canonical.dedup();
        if canonical.len() > MAX_ATTESTED_FILES {
            return Err(PluginProcessConfigError::AttestationLimit);
        }
        let mut total = 0_u64;
        let mut identities = Vec::with_capacity(canonical.len());
        for path in canonical {
            let identity = executable_identity(&path)?;
            if let Some(root) = &self.code_root
                && identity.canonical_path != self.executable
                && !identity.canonical_path.starts_with(&root.canonical_path)
            {
                return Err(PluginProcessConfigError::InvalidAttestedFile);
            }
            total = total
                .checked_add(identity.length)
                .ok_or(PluginProcessConfigError::AttestationLimit)?;
            if total > MAX_ATTESTED_BYTES {
                return Err(PluginProcessConfigError::AttestationLimit);
            }
            identities.push(identity);
        }
        self.attested_files = identities;
        Ok(self)
    }

    /// Pins the only plugin-owned code/package directory readable without a
    /// `reads-fs` capability. It must be a real non-symlink directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the root is a symlink, is not a directory, or
    /// cannot be resolved to a stable canonical identity.
    pub fn with_code_root(
        mut self,
        root: impl AsRef<Path>,
    ) -> Result<Self, PluginProcessConfigError> {
        self.code_root = Some(directory_identity(root.as_ref())?);
        Ok(self)
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn environment_allowlist(&self) -> &BTreeSet<OsString> {
        &self.environment_allowlist
    }

    #[must_use]
    pub fn allowed_domains(&self) -> &BTreeSet<String> {
        &self.allowed_domains
    }

    #[must_use]
    pub const fn env_clear(&self) -> bool {
        true
    }

    #[must_use]
    pub const fn executable_identity(&self) -> &ExecutableIdentity {
        &self.executable_identity
    }

    #[must_use]
    pub fn attested_files(&self) -> &[ExecutableIdentity] {
        &self.attested_files
    }

    #[must_use]
    pub const fn code_root(&self) -> Option<&CodeRootIdentity> {
        self.code_root.as_ref()
    }

    /// Revalidates the executable immediately before a launcher calls `exec`.
    ///
    /// # Errors
    ///
    /// Returns an error if the path was substituted after approval.
    pub fn validate_executable_identity(&self) -> Result<(), PluginProcessError> {
        let current =
            executable_identity(&self.executable).map_err(|error| PluginProcessError {
                message: error.to_string(),
            })?;
        if current != self.executable_identity {
            return Err(PluginProcessError {
                message: "approved plugin executable identity changed before exec".to_owned(),
            });
        }
        for expected in &self.attested_files {
            let current = executable_identity(&expected.canonical_path).map_err(|error| {
                PluginProcessError {
                    message: error.to_string(),
                }
            })?;
            if current != *expected {
                return Err(PluginProcessError {
                    message: "approved plugin content identity changed before exec".to_owned(),
                });
            }
        }
        if let Some(expected) = &self.code_root {
            let current = directory_identity(&expected.canonical_path).map_err(|error| {
                PluginProcessError {
                    message: error.to_string(),
                }
            })?;
            if current != *expected {
                return Err(PluginProcessError {
                    message: "approved plugin code-root identity changed before exec".to_owned(),
                });
            }
        }
        Ok(())
    }
}

fn directory_identity(path: &Path) -> Result<CodeRootIdentity, PluginProcessConfigError> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(PluginProcessConfigError::InvalidAttestedFile);
    }
    let canonical =
        std::fs::canonicalize(path).map_err(|_| PluginProcessConfigError::InvalidAttestedFile)?;
    let metadata =
        std::fs::metadata(&canonical).map_err(|_| PluginProcessConfigError::InvalidAttestedFile)?;
    if !metadata.is_dir() {
        return Err(PluginProcessConfigError::InvalidAttestedFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(CodeRootIdentity {
            canonical_path: canonical,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(CodeRootIdentity {
            canonical_path: canonical,
            device: 0,
            inode: 0,
        })
    }
}

fn executable_identity(path: &Path) -> Result<ExecutableIdentity, PluginProcessConfigError> {
    const MAX_IDENTITY_FILE_BYTES: u64 = 256 * 1024 * 1024;
    let metadata =
        std::fs::metadata(path).map_err(|_| PluginProcessConfigError::InvalidExecutable)?;
    if !metadata.is_file() || metadata.len() > MAX_IDENTITY_FILE_BYTES {
        return Err(PluginProcessConfigError::AttestationLimit);
    }
    let file =
        std::fs::File::open(path).map_err(|_| PluginProcessConfigError::InvalidExecutable)?;
    let mut file = file.take(metadata.len().saturating_add(1));
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut read_bytes = 0_u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| PluginProcessConfigError::InvalidExecutable)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        read_bytes = read_bytes.saturating_add(count as u64);
    }
    if read_bytes != metadata.len() {
        return Err(PluginProcessConfigError::InvalidExecutable);
    }
    let content_blake3 = hasher.finalize().to_hex().to_string();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(ExecutableIdentity {
            canonical_path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            content_blake3,
        })
    }
    #[cfg(not(unix))]
    {
        Ok(ExecutableIdentity {
            canonical_path: path.to_path_buf(),
            device: 0,
            inode: 0,
            length: metadata.len(),
            content_blake3,
        })
    }
}

fn validate_executable(executable: &Path) -> Result<PathBuf, PluginProcessConfigError> {
    if !executable.is_absolute()
        || executable.as_os_str().is_empty()
        || os_contains_nul(executable.as_os_str())
    {
        return Err(PluginProcessConfigError::InvalidExecutable);
    }
    let canonical = std::fs::canonicalize(executable)
        .map_err(|_| PluginProcessConfigError::InvalidExecutable)?;
    if !canonical.is_file() || !is_executable(&canonical) {
        return Err(PluginProcessConfigError::InvalidExecutable);
    }
    Ok(canonical)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

fn validate_cwd(cwd: &Path) -> Result<PathBuf, PluginProcessConfigError> {
    let canonical = std::fs::canonicalize(cwd)
        .map_err(|error| PluginProcessConfigError::InvalidCwd(error.to_string()))?;
    if !canonical.is_dir() {
        return Err(PluginProcessConfigError::InvalidCwd(
            "path is not a directory".to_owned(),
        ));
    }
    Ok(canonical)
}

#[cfg(unix)]
fn os_contains_nul(value: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().contains(&0)
}

#[cfg(not(unix))]
fn os_contains_nul(value: &OsStr) -> bool {
    value.to_string_lossy().contains('\0')
}

fn valid_environment_name(value: &OsStr) -> bool {
    let value = value.to_string_lossy();
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn safe_environment_name(value: &OsStr) -> bool {
    let value = value.to_string_lossy();
    matches!(
        value.as_ref(),
        "LANG" | "LC_ALL" | "LC_CTYPE" | "TERM" | "TZ" | "NO_COLOR" | "FORCE_COLOR"
    ) && !value.contains("KEY")
        && !value.contains("TOKEN")
        && !value.contains("SECRET")
        && !value.contains("PASSWORD")
        && !matches!(
            value.as_ref(),
            "LD_PRELOAD"
                | "LD_LIBRARY_PATH"
                | "DYLD_INSERT_LIBRARIES"
                | "DYLD_LIBRARY_PATH"
                | "NODE_OPTIONS"
                | "BUN_OPTIONS"
                | "PYTHONPATH"
                | "RUSTC_WRAPPER"
        )
}

fn valid_public_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.is_ascii()
        && !domain.ends_with('.')
        && domain.split('.').count() >= 2
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        && !matches!(domain, "localhost" | "localhost.localdomain")
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("plugin process error: {message}")]
pub struct PluginProcessError {
    pub message: String,
}

#[async_trait]
pub trait SupervisedPluginProcess: Send + Sync {
    /// Records why the process is untrusted before termination.
    fn mark_capability_violation(&self, violation: &CapabilityViolation);
    /// Terminates the plugin and its complete descendant process tree.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor cannot terminate the process tree.
    fn kill_tree(&self) -> Result<(), PluginProcessError>;

    /// Waits until the direct child exits and returns its exit code when available.
    async fn wait(&self) -> Result<Option<i32>, PluginProcessError> {
        Ok(None)
    }

    /// Reaps the direct child after termination.
    async fn reap(&self) -> Result<(), PluginProcessError> {
        let _ = self.wait().await?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityKind {
    Tool,
    Command,
    Hook,
    Provider,
    Event,
    Push,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("plugin attempted undeclared {kind:?} capability `{name}`")]
pub struct CapabilityViolation {
    pub kind: CapabilityKind,
    pub name: String,
}

/// Immutable manifest snapshot used to police every message after initialization.
pub struct CapabilityEnforcer {
    capabilities: PluginCapabilities,
    process: Arc<dyn SupervisedPluginProcess>,
    violated: AtomicBool,
    violation: Mutex<Option<CapabilityEnforcementError>>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{violation}{termination_suffix}", termination_suffix = .termination_error.as_ref().map(|value| format!("; termination failed: {}", value.message)).unwrap_or_default())]
pub struct CapabilityEnforcementError {
    pub violation: CapabilityViolation,
    pub termination_error: Option<PluginProcessError>,
}

impl CapabilityEnforcer {
    /// Creates an immutable capability snapshot for a supervised process.
    ///
    pub fn new(manifest: &PluginManifest, process: Arc<dyn SupervisedPluginProcess>) -> Self {
        Self {
            capabilities: manifest.capabilities.clone(),
            process,
            violated: AtomicBool::new(false),
            violation: Mutex::new(None),
        }
    }

    /// Verifies a tool declaration, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the tool was not declared.
    pub fn check_tool(&self, name: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Tool,
            name,
            self.capabilities.tools.iter().any(|tool| tool.name == name),
        )
    }

    #[must_use]
    pub fn tool_declaration_matches(&self, declaration: &PluginToolCapability) -> bool {
        self.capabilities
            .tools
            .iter()
            .any(|approved| approved == declaration)
    }

    /// Returns the effective authority held by the shared plugin process.
    ///
    /// A plugin process is one sandbox principal, so every tool adapter must
    /// present this union to the permission and checkpoint chokepoints rather
    /// than claiming only its handler's narrower declaration.
    #[must_use]
    pub fn process_tool_effects(&self) -> BTreeSet<PluginToolEffect> {
        let mut effects = self
            .capabilities
            .tools
            .iter()
            .flat_map(|tool| tool.caps.iter().copied())
            .collect::<BTreeSet<_>>();
        if !self.capabilities.providers.is_empty() {
            effects.insert(PluginToolEffect::Network);
        }
        effects
    }

    /// Verifies a command declaration, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the command was not declared.
    pub fn check_command(&self, name: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Command,
            name,
            self.capabilities
                .commands
                .iter()
                .any(|command| command.name == name),
        )
    }

    /// Verifies a hook declaration, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the hook was not declared.
    pub fn check_hook(&self, hook: PluginHook) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Hook,
            hook.as_str(),
            self.capabilities
                .hooks
                .iter()
                .any(|declaration| declaration.name() == hook),
        )
    }

    /// Verifies that a model alias matches a declared provider prefix.
    ///
    /// # Errors
    ///
    /// Returns a violation when no provider prefix matches.
    pub fn check_provider(&self, alias: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Provider,
            alias,
            self.capabilities
                .providers
                .iter()
                .any(|provider| alias.starts_with(&provider.alias_prefix)),
        )
    }

    /// Verifies an event subscription, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the event was not declared.
    pub fn check_event(&self, event: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Event,
            event,
            self.capabilities
                .event_subscriptions
                .iter()
                .any(|declared| declared == event),
        )
    }

    /// Verifies a typed push capability, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the push method was not declared.
    pub fn check_push(&self, method: PluginPush) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Push,
            method.method(),
            self.capabilities.push.contains(&method),
        )
    }

    /// Verifies a wire push method, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the push method was not declared.
    pub fn check_push_method(&self, method: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Push,
            method,
            self.capabilities
                .push
                .iter()
                .any(|declared| declared.method() == method),
        )
    }

    #[must_use]
    pub fn violated(&self) -> bool {
        self.violated.load(Ordering::Acquire)
    }

    fn check(
        &self,
        kind: CapabilityKind,
        name: &str,
        declared: bool,
    ) -> Result<(), CapabilityEnforcementError> {
        let cached_violation = {
            self.violation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        if let Some(mut error) = cached_violation {
            if error.termination_error.is_some() && self.process.kill_tree().is_ok() {
                error.termination_error = None;
                *self
                    .violation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
            }
            return Err(error);
        }
        if declared {
            return Ok(());
        }
        let violation = CapabilityViolation {
            kind,
            name: name.to_owned(),
        };
        let error = if self.violated.swap(true, Ordering::AcqRel) {
            CapabilityEnforcementError {
                violation,
                termination_error: None,
            }
        } else {
            self.process.mark_capability_violation(&violation);
            CapabilityEnforcementError {
                violation,
                termination_error: self.process.kill_tree().err(),
            }
        };
        *self
            .violation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
        Err(error)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("plugin RPC failed: {code}: {message}")]
pub struct PluginRpcError {
    pub code: String,
    pub message: String,
}

/// Incremental, request-scoped provider events received from a plugin. The
/// concrete transport owns cancellation and correlation cleanup when dropped.
pub type PluginProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<Value, PluginRpcError>> + Send + 'static>>;

#[async_trait]
pub trait PluginRpcClient: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value, PluginRpcError>;

    async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: &rw_tools::CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        if cancellation.is_cancelled() {
            return Err(PluginRpcError {
                code: "cancelled".to_owned(),
                message: "plugin RPC request was cancelled".to_owned(),
            });
        }
        self.request(method, params).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), PluginRpcError> {
        let _ = (method, params);
        Err(PluginRpcError {
            code: "unsupported".to_owned(),
            message: "RPC notifications are unsupported".to_owned(),
        })
    }

    /// Starts a provider request whose events arrive incrementally over the
    /// protocol's correlated `provider/event` notification channel.
    async fn provider_stream(
        &self,
        _params: Value,
    ) -> Result<PluginProviderEventStream, PluginRpcError> {
        Err(PluginRpcError {
            code: "unsupported".to_owned(),
            message: "RPC provider streaming is unsupported".to_owned(),
        })
    }
}

/// Adapter that registers an out-of-process hook through the common dispatcher.
pub struct RpcHookHandler {
    client: Arc<dyn PluginRpcClient>,
    enforcer: Arc<CapabilityEnforcer>,
}

impl RpcHookHandler {
    #[must_use]
    pub fn new(client: Arc<dyn PluginRpcClient>, enforcer: Arc<CapabilityEnforcer>) -> Self {
        Self { client, enforcer }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum RpcHookResponse {
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
    Deny {
        message: String,
    },
    Replace {
        payload: Value,
    },
}

#[async_trait]
impl HookHandler for RpcHookHandler {
    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        let hook = PluginHook::from(invocation.event());
        self.enforcer
            .check_hook(hook)
            .map_err(|error| HookError::new("capability_violation", error.to_string()))?;
        let result = self
            .client
            .request_cancellable(
                METHOD_HOOK_INVOKE,
                json!({
                    "hook": hook.as_str(),
                    "payload": invocation.payload(),
                }),
                invocation.cancellation(),
            )
            .await
            .map_err(|error| {
                if error.code.is_empty()
                    || error.code.len() > MAX_NAME_BYTES
                    || error.code.chars().any(char::is_control)
                    || error.message.is_empty()
                    || error.message.len() > MAX_RPC_MESSAGE_BYTES
                    || error.message.chars().any(char::is_control)
                {
                    HookError::new("invalid_rpc_error", "plugin returned an invalid RPC error")
                } else {
                    HookError::new(error.code, error.message)
                }
            })?;
        let mut result = result;
        if let Value::Object(object) = &mut result
            && !object.contains_key("decision")
            && let Some(action) = object.remove("action")
        {
            object.insert("decision".to_owned(), action);
        }
        let response: RpcHookResponse = serde_json::from_value(result)
            .map_err(|error| HookError::new("invalid_response", error.to_string()))?;
        match response {
            RpcHookResponse::Allow { payload: None } => Ok(HookDirective::Continue),
            RpcHookResponse::Allow {
                payload: Some(payload),
            }
            | RpcHookResponse::Replace { payload }
                if serde_json::to_vec(&payload)
                    .is_ok_and(|bytes| bytes.len() <= MAX_HOOK_PAYLOAD_BYTES) =>
            {
                Ok(HookDirective::Replace(payload))
            }
            RpcHookResponse::Allow { payload: Some(_) } | RpcHookResponse::Replace { .. } => {
                Err(HookError::new(
                    "invalid_response",
                    "hook replacement payload exceeds the limit",
                ))
            }
            RpcHookResponse::Deny { message }
                if !message.is_empty()
                    && message.len() <= MAX_RPC_MESSAGE_BYTES
                    && !message.chars().any(char::is_control) =>
            {
                Ok(HookDirective::Block { message })
            }
            RpcHookResponse::Deny { .. } => Err(HookError::new(
                "invalid_response",
                "hook denial message is invalid",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::{Mutex, atomic::AtomicUsize};

    use super::*;
    use crate::{HookDispatchStatus, HookDispatcher, HookRegistration};

    fn manifest() -> PluginManifest {
        PluginManifest {
            name: "safe-plugin".to_owned(),
            version: "1.0.0".to_owned(),
            protocol: PROTOCOL_VERSION,
            capabilities: PluginCapabilities {
                tools: vec![PluginToolCapability {
                    name: "read_custom".to_owned(),
                    description: "Read custom data".to_owned(),
                    schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
                    caps: vec![PluginToolEffect::ReadsFilesystem],
                }],
                hooks: vec![PluginHookDeclaration::Detailed(PluginHookCapability {
                    name: PluginHook::PreTool,
                    failure_policy: PluginHookFailurePolicy::FailClosed,
                })],
                providers: vec![PluginProviderCapability {
                    alias_prefix: "custom/".to_owned(),
                }],
                event_subscriptions: vec!["ToolCallFinished".to_owned()],
                push: vec![PluginPush::UiNotify],
                ..PluginCapabilities::default()
            },
        }
    }

    #[test]
    fn valid_manifest_has_order_independent_fingerprint() {
        let first = manifest();
        first.validate().expect("valid manifest");
        let mut second = first.clone();
        second.capabilities.hooks.reverse();
        second.capabilities.tools[0].caps.reverse();
        assert_eq!(
            first.fingerprint().expect("fingerprint"),
            second.fingerprint().expect("fingerprint")
        );
    }

    #[test]
    fn typescript_sdk_manifest_fixture_is_compatible() {
        let fixture = json!({
            "name": "sdk-fixture",
            "version": "1.0.0",
            "protocol": 1,
            "capabilities": {
                "tools": [{
                    "name": "hello",
                    "description": "Return a greeting",
                    "schema": {"type":"object","properties":{"name":{"type":"string"}}},
                    "caps": ["reads-fs", "network"]
                }],
                "commands": [{
                    "name": "greet",
                    "description": "Greet a user",
                    "argument_hint": "<name>",
                    "allowed_tools": ["hello"]
                }],
                "hooks": [{"name":"pre_tool","failure_policy":"fail-closed"}],
                "providers": [{"alias-prefix":"fixture/"}],
                "event_subscriptions": ["TurnFinished"]
            }
        });
        let bytes = serde_json::to_vec(&fixture).expect("fixture JSON");
        let manifest = PluginManifest::from_slice(&bytes).expect("TS SDK manifest");
        assert_eq!(manifest.capabilities.tools[0].caps.len(), 2);
        assert_eq!(
            manifest.capabilities.hooks[0].failure_policy(),
            PluginHookFailurePolicy::FailClosed
        );
    }

    #[test]
    fn language_neutral_protocol_fixture_matches_rust_constants() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../packages/plugin-sdk/fixtures/wire/protocol-1.json"
        ))
        .expect("protocol fixture JSON");
        assert_eq!(fixture["protocol"], PROTOCOL_VERSION);
        assert_eq!(fixture["limits"]["max_frame_bytes"], MAX_FRAME_BYTES);
        assert_eq!(fixture["limits"]["max_manifest_bytes"], MAX_MANIFEST_BYTES);
        assert_eq!(fixture["limits"]["max_version_bytes"], MAX_VERSION_BYTES);
        assert_eq!(fixture["methods"]["toolCall"], METHOD_TOOL_CALL);
        assert_eq!(
            fixture["methods"]["providerComplete"],
            METHOD_PROVIDER_COMPLETE
        );
        assert_eq!(fixture["methods"]["providerEvent"], METHOD_PROVIDER_EVENT);
        assert_eq!(fixture["methods"]["providerCancel"], METHOD_PROVIDER_CANCEL);
        assert_eq!(fixture["methods"]["notify"], METHOD_UI_NOTIFY);
    }

    #[test]
    fn decoder_rejects_oversized_and_malformed_frames() {
        let mut decoder = FrameDecoder::new(32);
        assert!(matches!(
            decoder.push(&[b'x'; 33]),
            Err(FrameError::TooLarge { limit: 32 })
        ));
        let mut decoder = FrameDecoder::default();
        assert!(matches!(
            decoder.push(b"{not-json}\n"),
            Err(FrameError::Malformed(_))
        ));
    }

    #[derive(Default)]
    struct MemoryApprovals(Mutex<BTreeMap<String, String>>);

    impl ApprovalStore for MemoryApprovals {
        fn approved_fingerprint(
            &self,
            plugin_name: &str,
        ) -> Result<Option<String>, ApprovalStoreError> {
            Ok(self.0.lock().expect("approvals").get(plugin_name).cloned())
        }

        fn record_approval(
            &self,
            plugin_name: &str,
            fingerprint: &str,
        ) -> Result<(), ApprovalStoreError> {
            self.0
                .lock()
                .expect("approvals")
                .insert(plugin_name.to_owned(), fingerprint.to_owned());
            Ok(())
        }
    }

    #[test]
    fn first_and_changed_manifests_require_approval() {
        let store = MemoryApprovals::default();
        let first = manifest();
        assert!(matches!(
            approval_requirement(&store, &first).expect("requirement"),
            ApprovalRequirement::FirstLoad { .. }
        ));
        approve_manifest(&store, &first).expect("approval");
        assert_eq!(
            approval_requirement(&store, &first).expect("requirement"),
            ApprovalRequirement::Approved
        );
        let mut changed = first;
        changed.capabilities.push.push(PluginPush::SessionSetStatus);
        assert!(matches!(
            approval_requirement(&store, &changed).expect("requirement"),
            ApprovalRequirement::ManifestChanged { .. }
        ));
    }

    #[derive(Default)]
    struct ProcessState {
        violations: Mutex<Vec<CapabilityViolation>>,
        killed: AtomicBool,
        kill_count: AtomicUsize,
    }

    impl SupervisedPluginProcess for ProcessState {
        fn mark_capability_violation(&self, violation: &CapabilityViolation) {
            self.violations
                .lock()
                .expect("violations")
                .push(violation.clone());
        }

        fn kill_tree(&self) -> Result<(), PluginProcessError> {
            self.killed.store(true, Ordering::Release);
            self.kill_count.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[test]
    fn undeclared_capability_marks_and_kills_process() {
        let process = Arc::new(ProcessState::default());
        let enforcer = CapabilityEnforcer::new(&manifest(), process.clone());
        assert!(enforcer.check_tool("secret_tool").is_err());
        assert!(enforcer.violated());
        assert!(process.killed.load(Ordering::Acquire));
        assert_eq!(process.violations.lock().expect("violations").len(), 1);
    }

    #[test]
    fn cached_violation_does_not_repeat_successful_process_termination() {
        let process = Arc::new(ProcessState::default());
        let enforcer = CapabilityEnforcer::new(&manifest(), process.clone());
        let first = enforcer
            .check_tool("secret_tool")
            .expect_err("undeclared tool");
        let cached = enforcer
            .check_command("secret_command")
            .expect_err("cached violation");
        assert_eq!(cached, first);
        assert_eq!(process.kill_count.load(Ordering::Acquire), 1);
        assert_eq!(process.violations.lock().expect("violations").len(), 1);
    }

    struct DenyClient;

    #[async_trait]
    impl PluginRpcClient for DenyClient {
        async fn request(&self, method: &str, params: Value) -> Result<Value, PluginRpcError> {
            assert_eq!(method, METHOD_HOOK_INVOKE);
            assert_eq!(params["hook"], "pre_tool");
            Ok(json!({"decision":"deny","message":"blocked by plugin"}))
        }
    }

    #[tokio::test]
    async fn pre_tool_deny_uses_common_hook_dispatcher() {
        let process = Arc::new(ProcessState::default());
        let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process));
        let handler = RpcHookHandler::new(Arc::new(DenyClient), enforcer);
        let mut dispatcher = HookDispatcher::new();
        dispatcher
            .register(
                HookRegistration::new("plugin:pre-tool", HookEvent::PreTool),
                handler,
            )
            .expect("hook registration");
        let result = dispatcher
            .dispatch(HookEvent::PreTool, json!({"name":"write"}))
            .await;
        assert_eq!(
            result.status(),
            &HookDispatchStatus::Blocked {
                hook_id: "plugin:pre-tool".to_owned(),
                message: "blocked by plugin".to_owned(),
            }
        );
    }
}
