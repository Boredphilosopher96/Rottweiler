use std::collections::BTreeMap;
use std::path::Path;

use rw_tools::validate_mcp_virtual_tool;
use rw_types::{ModeId, SessionMode};
use serde::Deserialize;
use thiserror::Error;

use crate::ArtifactOrigin;

const MAX_MODE_ID_BYTES: usize = 64;
const MAX_PROMPT_BYTES: usize = 16 * 1024;
const MAX_ALLOWED_TOOLS: usize = 128;

/// Provenance of a declarative mode definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModeSource {
    Embedded { name: String },
    Artifact(ArtifactOrigin),
}

/// Parsed declarative mode. Embedded and filesystem definitions use the same parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModeDefinition {
    id: ModeId,
    description: String,
    permission: SessionMode,
    prompt: String,
    allowed_tools: Vec<String>,
    source: ModeSource,
}

impl ModeDefinition {
    #[must_use]
    pub fn id(&self) -> &ModeId {
        &self.id
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub const fn permission(&self) -> SessionMode {
        self.permission
    }

    #[must_use]
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Empty means that the mode does not further narrow the session tool registry.
    #[must_use]
    pub fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }

    #[must_use]
    pub const fn source(&self) -> &ModeSource {
        &self.source
    }

    /// Stable hash of the security-relevant declarative semantics. Provenance
    /// and filesystem paths are deliberately excluded so the value is safe to
    /// persist and portable across machines.
    #[must_use]
    pub fn semantic_fingerprint(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rottweiler-mode-v1\0");
        for field in [
            self.id.0.as_bytes(),
            self.description.as_bytes(),
            self.permission.as_str().as_bytes(),
            self.prompt.as_bytes(),
        ] {
            hasher.update(&u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
            hasher.update(field);
        }
        for tool in &self.allowed_tools {
            hasher.update(&u64::try_from(tool.len()).unwrap_or(u64::MAX).to_le_bytes());
            hasher.update(tool.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct ModeDocument {
    id: String,
    description: String,
    permission: SessionMode,
    prompt: String,
    #[serde(default)]
    allowed_tools: Vec<String>,
}

/// Parses one mode TOML document. This is the sole parser used for embedded
/// built-ins and discovered third-party definitions.
///
/// # Errors
///
/// Returns an error for malformed TOML, unknown fields, unsafe identifiers,
/// oversized prompt text, or invalid tool allowlist entries.
pub fn parse_mode_toml(
    source_name: &str,
    contents: &str,
    source: ModeSource,
) -> Result<ModeDefinition, ModeRegistryError> {
    let document = toml::from_str::<ModeDocument>(contents).map_err(|error| {
        ModeRegistryError::InvalidDocument {
            location: source_name.to_owned(),
            message: error.to_string(),
        }
    })?;
    validate_mode_id(&document.id).map_err(|message| ModeRegistryError::InvalidDocument {
        location: source_name.to_owned(),
        message,
    })?;
    if document.description.trim().is_empty()
        || document.description.len() > 512
        || document.description.chars().any(char::is_control)
    {
        return Err(ModeRegistryError::InvalidDocument {
            location: source_name.to_owned(),
            message: "description must contain 1-512 bytes and no control characters".to_owned(),
        });
    }
    if document.prompt.trim().is_empty()
        || document.prompt.len() > MAX_PROMPT_BYTES
        || document.prompt.contains('\0')
    {
        return Err(ModeRegistryError::InvalidDocument {
            location: source_name.to_owned(),
            message: format!("prompt must contain 1-{MAX_PROMPT_BYTES} bytes and no NUL bytes"),
        });
    }
    if document.allowed_tools.len() > MAX_ALLOWED_TOOLS {
        return Err(ModeRegistryError::InvalidDocument {
            location: source_name.to_owned(),
            message: format!("allowed-tools exceeds the {MAX_ALLOWED_TOOLS}-entry limit"),
        });
    }
    for tool in &document.allowed_tools {
        let canonical = !tool.is_empty()
            && tool
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if !canonical && validate_mcp_virtual_tool(tool).is_err() {
            return Err(ModeRegistryError::InvalidDocument {
                location: source_name.to_owned(),
                message: format!("invalid allowed tool {tool:?}"),
            });
        }
    }
    let mut allowed_tools = document.allowed_tools;
    allowed_tools.sort();
    allowed_tools.dedup();
    Ok(ModeDefinition {
        id: ModeId(document.id),
        description: document.description,
        permission: document.permission,
        prompt: document.prompt,
        allowed_tools,
        source,
    })
}

fn validate_mode_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > MAX_MODE_ID_BYTES
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        || !id.as_bytes()[0].is_ascii_lowercase()
    {
        return Err(format!(
            "mode id must be 1-{MAX_MODE_ID_BYTES} bytes, start with a lowercase letter, and contain only lowercase letters, digits, '-' or '_'"
        ));
    }
    Ok(())
}

/// Stable mode registry shared by built-ins and extensions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModeRegistry {
    definitions: BTreeMap<String, ModeDefinition>,
}

impl ModeRegistry {
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ModeDefinition> {
        self.definitions.get(id)
    }

    /// Registers one unique mode definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the id is already registered.
    pub fn register(&mut self, definition: ModeDefinition) -> Result<(), ModeRegistryError> {
        let id = definition.id.0.clone();
        if self.definitions.contains_key(&id) {
            return Err(ModeRegistryError::Duplicate(id));
        }
        self.definitions.insert(id, definition);
        Ok(())
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ModeDefinition> {
        self.definitions.values()
    }

    /// Returns a registry with one already-active definition pinned over later
    /// discovery generations. This keeps a durable session's policy stable
    /// when workspace roots are appended.
    #[must_use]
    pub fn with_pinned(&self, definition: ModeDefinition) -> Self {
        let mut registry = self.clone();
        registry
            .definitions
            .insert(definition.id.0.clone(), definition);
        registry
    }

    /// Parses embedded built-ins through [`parse_mode_toml`].
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded asset fails the same public parser or
    /// conflicts with another embedded definition.
    pub fn builtins() -> Result<Self, ModeRegistryError> {
        let mut registry = Self::default();
        for (name, contents) in [
            ("discuss.toml", include_str!("../assets/modes/discuss.toml")),
            ("plan.toml", include_str!("../assets/modes/plan.toml")),
            ("execute.toml", include_str!("../assets/modes/execute.toml")),
        ] {
            registry.register(parse_mode_toml(
                name,
                contents,
                ModeSource::Embedded {
                    name: name.to_owned(),
                },
            )?)?;
        }
        Ok(registry)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ModeRegistryError {
    #[error("invalid mode definition `{location}`: {message}")]
    InvalidDocument { location: String, message: String },
    #[error("mode `{0}` is already registered")]
    Duplicate(String),
}

/// Composes discovered definitions with embedded built-ins. The built-in ids
/// are reserved because their names carry security-significant permission
/// guarantees; ADR-014 precedence still applies among custom definitions.
///
/// # Errors
///
/// Returns an error if discovered definitions conflict or an embedded asset is invalid.
pub fn compose_mode_registry(
    catalog: &crate::ExtensionCatalog,
) -> Result<ModeRegistry, ModeRegistryError> {
    let mut registry = ModeRegistry::builtins()?;
    for definition in catalog.modes() {
        registry.register(definition.clone())?;
    }
    Ok(registry)
}

/// Reads a bounded mode source and parses it through the public parser.
pub(crate) fn parse_mode_file(
    root: &Path,
    path: &Path,
    origin: ArtifactOrigin,
) -> Result<ModeDefinition, crate::ExtensionDiscoveryError> {
    let relative =
        path.strip_prefix(root)
            .map_err(|_| crate::ExtensionDiscoveryError::InvalidPath {
                path: path.to_owned(),
            })?;
    let contents = crate::discovery::read_bounded_relative_utf8(
        root,
        relative,
        crate::discovery::MAX_MARKDOWN_BYTES,
    )?;
    let definition = parse_mode_toml(
        &path.display().to_string(),
        &contents,
        ModeSource::Artifact(origin),
    )
    .map_err(|error| crate::ExtensionDiscoveryError::InvalidMode {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| crate::ExtensionDiscoveryError::InvalidPath {
            path: path.to_owned(),
        })?;
    if stem != definition.id().0 {
        return Err(crate::ExtensionDiscoveryError::InvalidMode {
            path: path.to_owned(),
            message: "`id` must match the file name".to_owned(),
        });
    }
    Ok(definition)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn embedded_modes_use_public_parser_and_have_distinct_policies() {
        let registry = ModeRegistry::builtins().expect("built-ins");
        assert_eq!(registry.iter().len(), 3);
        assert_eq!(
            registry.get("plan").expect("plan").permission(),
            SessionMode::Plan
        );
        assert!(
            registry
                .get("discuss")
                .expect("discuss")
                .prompt()
                .contains("Discuss")
        );
        assert!(matches!(
            registry.get("execute").expect("execute").source(),
            ModeSource::Embedded { .. }
        ));
    }

    #[test]
    fn parser_rejects_unknown_fields_and_unsafe_tool_names() {
        let invalid = r#"
id = "audit"
description = "Audit"
permission = "discuss"
prompt = "Read only"
allowed-tools = ["Shell Please"]
surprise = true
"#;
        assert!(
            parse_mode_toml(
                "audit.toml",
                invalid,
                ModeSource::Embedded {
                    name: "test".to_owned()
                }
            )
            .is_err()
        );
        for unsafe_text in [
            "id = \"audit\"\ndescription = \"bad\\u0001label\"\npermission = \"discuss\"\nprompt = \"Read only\"\n",
            "id = \"audit\"\ndescription = \"Audit\"\npermission = \"discuss\"\nprompt = \"bad\\u0000prompt\"\n",
        ] {
            assert!(
                parse_mode_toml(
                    "audit.toml",
                    unsafe_text,
                    ModeSource::Embedded {
                        name: "test".to_owned(),
                    },
                )
                .is_err()
            );
        }
    }

    #[test]
    fn semantic_fingerprint_is_source_independent_and_policy_sensitive() {
        let contents = r#"
id = "audit"
description = "Audit"
permission = "discuss"
prompt = "Read only"
allowed-tools = ["grep", "read"]
"#;
        let embedded = parse_mode_toml(
            "embedded.toml",
            contents,
            ModeSource::Embedded {
                name: "embedded".to_owned(),
            },
        )
        .expect("embedded mode");
        let artifact = parse_mode_toml(
            "artifact.toml",
            contents,
            ModeSource::Artifact(ArtifactOrigin::new(
                crate::ArtifactScope::User,
                crate::ArtifactLocation::Agents,
                Path::new("/private/path/audit.toml").to_owned(),
            )),
        )
        .expect("artifact mode");
        assert_eq!(
            embedded.semantic_fingerprint(),
            artifact.semantic_fingerprint()
        );
        assert_eq!(embedded.semantic_fingerprint().len(), 64);
        assert_eq!(
            embedded.semantic_fingerprint(),
            "71a6da2ec931ddc5ef8d6f68d771ade2dcd114d5b616caa8975fe433143fb9c0"
        );

        let changed = parse_mode_toml(
            "changed.toml",
            &contents.replace("permission = \"discuss\"", "permission = \"execute\""),
            ModeSource::Embedded {
                name: "changed".to_owned(),
            },
        )
        .expect("changed mode");
        assert_ne!(
            embedded.semantic_fingerprint(),
            changed.semantic_fingerprint()
        );
    }
}
