//! Declarative command and skill discovery.
//!
//! Discovery reads metadata, but it never evaluates command templates or
//! project content. Shell interpolation and file inclusion remain typed lazy
//! template parts for the engine permission/trust layers to resolve later.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use crate::CommandDescriptor;

mod shell_hook;

pub use shell_hook::DiscoveredShellHook;

const MAX_MARKDOWN_BYTES: u64 = 1024 * 1024;
const MAX_RESOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// Code-distributed migration table for Claude-style command frontmatter.
/// Values marked `unchanged` use the same spelling in Rottweiler.
pub const CLAUDE_FRONTMATTER_MIGRATION: &[(&str, &str)] = &[
    ("description", "description (unchanged)"),
    ("model", "model (unchanged)"),
    ("allowed-tools", "allowed-tools (unchanged)"),
    ("args", "argument-hint"),
];

/// Whether an artifact came from the project or user configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactScope {
    Project,
    User,
}

/// The open or Rottweiler-specific declarative location.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactLocation {
    Agents,
    Rottweiler,
}

/// Stable provenance attached to every discovered artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactOrigin {
    scope: ArtifactScope,
    location: ArtifactLocation,
    path: PathBuf,
}

impl ArtifactOrigin {
    /// Project or user scope.
    #[must_use]
    pub const fn scope(&self) -> ArtifactScope {
        self.scope
    }

    /// `.agents` or `.rottweiler` source.
    #[must_use]
    pub const fn location(&self) -> ArtifactLocation {
        self.location
    }

    /// Exact source file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Declarative artifact category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactKind {
    Command,
    Skill,
    Hook,
}

/// An untrusted project artifact visible to the folder-trust inventory.
///
/// These entries are deliberately not present in the active command/skill
/// maps. User-level artifacts therefore remain available in an untrusted
/// workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InertProjectArtifact {
    kind: ArtifactKind,
    name: String,
    path: PathBuf,
    contains_shell_interpolation: bool,
    executes_command: bool,
}

impl InertProjectArtifact {
    #[must_use]
    pub const fn kind(&self) -> ArtifactKind {
        self.kind
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the command source advertises lazy shell interpolation.
    #[must_use]
    pub const fn contains_shell_interpolation(&self) -> bool {
        self.contains_shell_interpolation
    }

    /// Whether activating this artifact makes a configured shell command
    /// eligible for later permission-checked execution.
    #[must_use]
    pub const fn executes_command(&self) -> bool {
        self.executes_command
    }
}

/// Roots and trust state used for one deterministic discovery pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionDiscoveryConfig {
    project_root: PathBuf,
    additional_project_roots: Vec<(PathBuf, bool)>,
    user_home: PathBuf,
    user_rottweiler_root: PathBuf,
    project_trusted: bool,
}

impl ExtensionDiscoveryConfig {
    #[must_use]
    pub fn new(project_root: impl Into<PathBuf>, user_home: impl Into<PathBuf>) -> Self {
        let user_home = user_home.into();
        Self {
            project_root: project_root.into(),
            additional_project_roots: Vec::new(),
            user_rottweiler_root: user_home.join(".rottweiler"),
            user_home,
            project_trusted: false,
        }
    }

    /// Marks project artifacts active for this pass. Callers must derive this
    /// only from the M5 folder-trust assessment.
    #[must_use]
    pub const fn with_project_trusted(mut self, trusted: bool) -> Self {
        self.project_trusted = trusted;
        self
    }

    /// Adds another stable-index workspace root and its independent trust
    /// state. Earlier project roots retain precedence.
    #[must_use]
    pub fn with_additional_project_root(mut self, root: impl Into<PathBuf>, trusted: bool) -> Self {
        self.additional_project_roots.push((root.into(), trusted));
        self
    }

    /// Overrides the effective user Rottweiler directory while keeping open
    /// `~/.agents` discovery rooted at `user_home`.
    #[must_use]
    pub fn with_user_rottweiler_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.user_rottweiler_root = root.into();
        self
    }

    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    #[must_use]
    pub fn user_home(&self) -> &Path {
        &self.user_home
    }

    #[must_use]
    pub const fn project_trusted(&self) -> bool {
        self.project_trusted
    }
}

/// One lazy prompt-template operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TemplatePart {
    Text(String),
    Arguments,
    PositionalArgument(usize),
    /// Shell text to be permission-checked and executed by the engine later.
    ShellInterpolation {
        command: String,
    },
    /// A workspace-relative file request to be resolved by the engine later.
    FileInclusion {
        path: String,
    },
}

/// Parsed command body with all side-effecting operations still inert.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandTemplate {
    parts: Vec<TemplatePart>,
}

impl CommandTemplate {
    #[must_use]
    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    #[must_use]
    pub fn requires_shell(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, TemplatePart::ShellInterpolation { .. }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LazyMarkdownBody {
    path: PathBuf,
    digest: blake3::Hash,
}

impl LazyMarkdownBody {
    fn load(&self) -> Result<String, ExtensionDiscoveryError> {
        let contents = read_bounded_utf8(&self.path, MAX_MARKDOWN_BYTES)?;
        if blake3::hash(contents.as_bytes()) != self.digest {
            return Err(ExtensionDiscoveryError::ChangedAfterDiscovery {
                path: self.path.clone(),
            });
        }
        let document = parse_frontmatter(&self.path, &contents)?;
        Ok(document.body.to_owned())
    }
}

/// A discovered user-authored slash command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredCommand {
    name: String,
    description: String,
    model: Option<String>,
    allowed_tools: Vec<String>,
    argument_hint: Option<String>,
    used_legacy_args_alias: bool,
    origin: ArtifactOrigin,
    body: LazyMarkdownBody,
}

impl DiscoveredCommand {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }

    #[must_use]
    pub fn argument_hint(&self) -> Option<&str> {
        self.argument_hint.as_deref()
    }

    /// `args` is accepted as a migration alias for `argument-hint`.
    #[must_use]
    pub const fn used_legacy_args_alias(&self) -> bool {
        self.used_legacy_args_alias
    }

    #[must_use]
    pub const fn origin(&self) -> &ArtifactOrigin {
        &self.origin
    }

    /// Metadata suitable for registration on the shared M2 registry.
    #[must_use]
    pub fn descriptor(&self) -> CommandDescriptor {
        let descriptor = CommandDescriptor::new(&self.name, &self.description);
        match &self.argument_hint {
            Some(hint) => descriptor.with_argument_hint(hint),
            None => descriptor,
        }
    }

    /// Loads and parses the body without executing interpolation or reading
    /// included files.
    ///
    /// # Errors
    ///
    /// Fails if the source changed after discovery, is unreadable, or contains
    /// an unterminated shell interpolation.
    pub fn load_template(&self) -> Result<CommandTemplate, ExtensionDiscoveryError> {
        parse_template(&self.origin.path, &self.body.load()?)
    }
}

/// A lazily loadable file bundled with a skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillResource {
    skill_root: PathBuf,
    relative_path: PathBuf,
}

impl SkillResource {
    #[must_use]
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    /// Reads this resource only when explicitly invoked by the skill loader.
    ///
    /// # Errors
    ///
    /// Rejects traversal, symlinks, non-files, and oversized resources.
    pub fn load(&self) -> Result<LoadedSkillResource, ExtensionDiscoveryError> {
        validate_relative_resource(&self.relative_path)?;
        let bytes =
            read_bounded_relative_file(&self.skill_root, &self.relative_path, MAX_RESOURCE_BYTES)?;
        Ok(LoadedSkillResource {
            relative_path: self.relative_path.clone(),
            bytes,
        })
    }
}

/// Bytes returned by an explicit lazy skill-resource load.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedSkillResource {
    relative_path: PathBuf,
    bytes: Vec<u8>,
}

impl LoadedSkillResource {
    #[must_use]
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// SKILL.md metadata. Instructions and bundled files stay lazy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredSkill {
    name: String,
    description: String,
    allowed_tools: Vec<String>,
    origin: ArtifactOrigin,
    root: PathBuf,
    body: LazyMarkdownBody,
}

impl DiscoveredSkill {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }

    #[must_use]
    pub const fn origin(&self) -> &ArtifactOrigin {
        &self.origin
    }

    /// Loads the instruction body only when the skill is invoked.
    ///
    /// # Errors
    ///
    /// Fails closed when SKILL.md changed after discovery.
    pub fn load_instructions(&self) -> Result<String, ExtensionDiscoveryError> {
        self.body.load()
    }

    /// Enumerates bundled files without reading their contents. `SKILL.md` is
    /// excluded and symlinks are rejected.
    ///
    /// # Errors
    ///
    /// Fails on unreadable directories, symlinks, or unsupported entries.
    pub fn resources(&self) -> Result<Vec<SkillResource>, ExtensionDiscoveryError> {
        let mut paths = Vec::new();
        collect_resource_paths(&self.root, &self.root, &mut paths)?;
        paths.sort();
        Ok(paths
            .into_iter()
            .filter(|path| path != Path::new("SKILL.md"))
            .map(|relative_path| SkillResource {
                skill_root: self.root.clone(),
                relative_path,
            })
            .collect())
    }
}

/// Active declarative extensions plus the project entries held behind trust.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExtensionCatalog {
    commands: BTreeMap<String, DiscoveredCommand>,
    skills: BTreeMap<String, DiscoveredSkill>,
    shell_hooks: Vec<DiscoveredShellHook>,
    inert_project_artifacts: Vec<InertProjectArtifact>,
}

impl ExtensionCatalog {
    /// Discovers commands and skills in ADR-014 order. First active match by
    /// name wins: project `.agents`, project `.rottweiler`, user `.agents`,
    /// user `.rottweiler`. An untrusted project is inventoried but skipped for
    /// active resolution.
    ///
    /// # Errors
    ///
    /// Returns a path-specific error for malformed or unsafe active sources.
    pub fn discover(config: &ExtensionDiscoveryConfig) -> Result<Self, ExtensionDiscoveryError> {
        let user_sources = [
            (ArtifactLocation::Agents, config.user_home.join(".agents")),
            (
                ArtifactLocation::Rottweiler,
                config.user_rottweiler_root.clone(),
            ),
        ];
        let mut catalog = Self::default();

        for (project_root, trusted) in
            std::iter::once((config.project_root.as_path(), config.project_trusted)).chain(
                config
                    .additional_project_roots
                    .iter()
                    .map(|(root, trusted)| (root.as_path(), *trusted)),
            )
        {
            let project_sources = [
                (ArtifactLocation::Agents, project_root.join(".agents")),
                (
                    ArtifactLocation::Rottweiler,
                    project_root.join(".rottweiler"),
                ),
            ];
            if trusted {
                for (location, root) in project_sources {
                    catalog.discover_active_root(ArtifactScope::Project, location, &root)?;
                }
            } else {
                for (_, root) in project_sources {
                    catalog.inventory_inert_project_root(&root)?;
                }
            }
        }
        for (location, root) in user_sources {
            catalog.discover_active_root(ArtifactScope::User, location, &root)?;
        }
        catalog.shell_hooks.sort_by(|left, right| {
            left.registration()
                .priority()
                .cmp(&right.registration().priority())
                .then_with(|| left.id().cmp(right.id()))
        });
        Ok(catalog)
    }

    #[must_use]
    pub fn command(&self, name: &str) -> Option<&DiscoveredCommand> {
        self.commands.get(name.strip_prefix('/').unwrap_or(name))
    }

    #[must_use]
    pub fn skill(&self, name: &str) -> Option<&DiscoveredSkill> {
        self.skills.get(name)
    }

    /// Active commands in stable name order.
    #[must_use]
    pub fn commands(&self) -> impl ExactSizeIterator<Item = &DiscoveredCommand> {
        self.commands.values()
    }

    /// Active skills in stable name order.
    #[must_use]
    pub fn skills(&self) -> impl ExactSizeIterator<Item = &DiscoveredSkill> {
        self.skills.values()
    }

    /// Active declarative hooks in dispatcher order `(priority, id)`.
    #[must_use]
    pub fn shell_hooks(&self) -> &[DiscoveredShellHook] {
        &self.shell_hooks
    }

    #[must_use]
    pub fn shell_hook(&self, id: &str) -> Option<&DiscoveredShellHook> {
        self.shell_hooks.iter().find(|hook| hook.id() == id)
    }

    #[must_use]
    pub fn inert_project_artifacts(&self) -> &[InertProjectArtifact] {
        &self.inert_project_artifacts
    }

    fn discover_active_root(
        &mut self,
        scope: ArtifactScope,
        location: ArtifactLocation,
        root: &Path,
    ) -> Result<(), ExtensionDiscoveryError> {
        for path in regular_children_with_extension(&root.join("commands"), "md")? {
            let command = discover_command(scope, location, &path)?;
            self.commands.entry(command.name.clone()).or_insert(command);
        }
        for path in skill_manifests(&root.join("skills"))? {
            let skill = discover_skill(scope, location, &path)?;
            self.skills.entry(skill.name.clone()).or_insert(skill);
        }
        let hooks_path = root.join("hooks.toml");
        if hooks_path.exists() {
            for hook in shell_hook::discover_file(scope, location, &hooks_path)? {
                if self
                    .shell_hooks
                    .iter()
                    .all(|existing| existing.id() != hook.id())
                {
                    self.shell_hooks.push(hook);
                }
            }
        }
        Ok(())
    }

    fn inventory_inert_project_root(&mut self, root: &Path) -> Result<(), ExtensionDiscoveryError> {
        for path in regular_children_with_extension(&root.join("commands"), "md")? {
            let contents = read_bounded_utf8(&path, MAX_MARKDOWN_BYTES)?;
            self.inert_project_artifacts.push(InertProjectArtifact {
                kind: ArtifactKind::Command,
                name: file_stem(&path)?,
                path,
                contains_shell_interpolation: contents.contains("!`"),
                executes_command: contents.contains("!`"),
            });
        }
        for path in skill_manifests(&root.join("skills"))? {
            self.inert_project_artifacts.push(InertProjectArtifact {
                kind: ArtifactKind::Skill,
                name: path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| ExtensionDiscoveryError::InvalidPath { path: path.clone() })?
                    .to_owned(),
                path,
                contains_shell_interpolation: false,
                executes_command: false,
            });
        }
        let hooks_path = root.join("hooks.toml");
        if hooks_path.exists() {
            ensure_regular_file(&hooks_path)?;
            self.inert_project_artifacts.push(InertProjectArtifact {
                kind: ArtifactKind::Hook,
                name: "hooks".to_owned(),
                path: hooks_path,
                contains_shell_interpolation: false,
                executes_command: true,
            });
        }
        Ok(())
    }
}

/// Discovery, parsing, or lazy-load failure.
#[derive(Debug, Error)]
pub enum ExtensionDiscoveryError {
    #[error("failed to inspect extension path `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("extension path `{path}` is not a regular file or directory of the expected kind")]
    UnsafeEntry { path: PathBuf },
    #[error("extension file `{path}` exceeds the {limit}-byte limit")]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("extension file `{path}` is not UTF-8")]
    NotUtf8 { path: PathBuf },
    #[error("extension path `{path}` has no portable UTF-8 name")]
    InvalidPath { path: PathBuf },
    #[error("markdown extension `{path}` must start with `---` frontmatter")]
    MissingFrontmatter { path: PathBuf },
    #[error("markdown extension `{path}` has unterminated frontmatter")]
    UnterminatedFrontmatter { path: PathBuf },
    #[error("invalid frontmatter in `{path}` at line {line}: {message}")]
    InvalidFrontmatter {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error("required frontmatter field `{field}` is missing in `{path}`")]
    MissingField { path: PathBuf, field: &'static str },
    #[error("invalid extension name `{name}` in `{path}`")]
    InvalidName { path: PathBuf, name: String },
    #[error("extension source `{path}` changed after discovery; rediscover and re-check trust")]
    ChangedAfterDiscovery { path: PathBuf },
    #[error("unterminated shell interpolation in `{path}`")]
    UnterminatedShellInterpolation { path: PathBuf },
    #[error("skill resource path `{path}` must be relative and may not traverse parents")]
    InvalidResourcePath { path: PathBuf },
    #[error("invalid hooks TOML in `{path}`: {message}")]
    InvalidHooksToml { path: PathBuf, message: String },
    #[error("invalid hook #{index} in `{path}`: {message}")]
    InvalidHook {
        path: PathBuf,
        index: usize,
        message: String,
    },
}

#[derive(Debug)]
struct FrontmatterDocument<'a> {
    fields: BTreeMap<String, FrontmatterValue>,
    body: &'a str,
}

#[derive(Debug)]
enum FrontmatterValue {
    Scalar(String),
    List(Vec<String>),
}

fn discover_command(
    scope: ArtifactScope,
    location: ArtifactLocation,
    path: &Path,
) -> Result<DiscoveredCommand, ExtensionDiscoveryError> {
    let contents = read_bounded_utf8(path, MAX_MARKDOWN_BYTES)?;
    let digest = blake3::hash(contents.as_bytes());
    let document = parse_frontmatter(path, &contents)?;
    let name = file_stem(path)?;
    validate_artifact_name(path, &name)?;
    let description = required_scalar(path, &document.fields, "description")?;
    let model = optional_scalar(path, &document.fields, "model")?;
    let allowed_tools = optional_list(&document.fields, "allowed-tools");
    let canonical_hint = optional_scalar(path, &document.fields, "argument-hint")?;
    let legacy_hint = optional_scalar(path, &document.fields, "args")?;
    let used_legacy_args_alias = canonical_hint.is_none() && legacy_hint.is_some();
    let argument_hint = canonical_hint.or(legacy_hint);
    Ok(DiscoveredCommand {
        name,
        description,
        model,
        allowed_tools,
        argument_hint,
        used_legacy_args_alias,
        origin: ArtifactOrigin {
            scope,
            location,
            path: path.to_owned(),
        },
        body: LazyMarkdownBody {
            path: path.to_owned(),
            digest,
        },
    })
}

fn discover_skill(
    scope: ArtifactScope,
    location: ArtifactLocation,
    path: &Path,
) -> Result<DiscoveredSkill, ExtensionDiscoveryError> {
    let contents = read_bounded_utf8(path, MAX_MARKDOWN_BYTES)?;
    let digest = blake3::hash(contents.as_bytes());
    let document = parse_frontmatter(path, &contents)?;
    let name = required_scalar(path, &document.fields, "name")?;
    validate_artifact_name(path, &name)?;
    let description = required_scalar(path, &document.fields, "description")?;
    let allowed_tools = optional_list(&document.fields, "allowed-tools");
    let root = path
        .parent()
        .ok_or_else(|| ExtensionDiscoveryError::InvalidPath {
            path: path.to_owned(),
        })?
        .to_owned();
    Ok(DiscoveredSkill {
        name,
        description,
        allowed_tools,
        origin: ArtifactOrigin {
            scope,
            location,
            path: path.to_owned(),
        },
        root,
        body: LazyMarkdownBody {
            path: path.to_owned(),
            digest,
        },
    })
}

fn parse_frontmatter<'a>(
    path: &Path,
    contents: &'a str,
) -> Result<FrontmatterDocument<'a>, ExtensionDiscoveryError> {
    let normalized = contents.strip_prefix("\u{feff}").unwrap_or(contents);
    let mut offset = 0;
    let mut lines = normalized.split_inclusive('\n');
    let first = lines
        .next()
        .ok_or_else(|| ExtensionDiscoveryError::MissingFrontmatter {
            path: path.to_owned(),
        })?;
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Err(ExtensionDiscoveryError::MissingFrontmatter {
            path: path.to_owned(),
        });
    }
    offset += first.len();
    let mut frontmatter_lines = Vec::new();
    let mut closed = false;
    for (index, line) in lines.enumerate() {
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            closed = true;
            break;
        }
        frontmatter_lines.push((index + 2, line.trim_end_matches(['\r', '\n'])));
    }
    if !closed {
        return Err(ExtensionDiscoveryError::UnterminatedFrontmatter {
            path: path.to_owned(),
        });
    }
    let fields = parse_frontmatter_fields(path, &frontmatter_lines)?;
    Ok(FrontmatterDocument {
        fields,
        body: &normalized[offset..],
    })
}

fn parse_frontmatter_fields(
    path: &Path,
    lines: &[(usize, &str)],
) -> Result<BTreeMap<String, FrontmatterValue>, ExtensionDiscoveryError> {
    let mut fields = BTreeMap::new();
    let mut index = 0;
    while index < lines.len() {
        let (line_number, raw) = lines[index];
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        if raw.starts_with(char::is_whitespace) {
            return invalid_frontmatter(path, line_number, "unexpected indentation");
        }
        let Some((raw_key, raw_value)) = line.split_once(':') else {
            return invalid_frontmatter(path, line_number, "expected `key: value`");
        };
        let key = raw_key.trim();
        if !valid_frontmatter_key(key) {
            return invalid_frontmatter(path, line_number, "invalid field name");
        }
        if fields.contains_key(key) {
            return invalid_frontmatter(path, line_number, "duplicate field");
        }
        let raw_value = raw_value.trim();
        if raw_value.is_empty() {
            let mut values = Vec::new();
            index += 1;
            while index < lines.len() {
                let (item_line, item_raw) = lines[index];
                if item_raw.trim().is_empty() {
                    index += 1;
                    continue;
                }
                let item = item_raw.trim_start();
                if !item_raw.starts_with(char::is_whitespace) || !item.starts_with('-') {
                    break;
                }
                let value = item[1..].trim();
                if value.is_empty() {
                    return invalid_frontmatter(path, item_line, "empty list item");
                }
                values.push(parse_scalar(path, item_line, value)?);
                index += 1;
            }
            fields.insert(key.to_owned(), FrontmatterValue::List(values));
            continue;
        }
        let value = if raw_value.starts_with('[') {
            FrontmatterValue::List(parse_inline_list(path, line_number, raw_value)?)
        } else {
            FrontmatterValue::Scalar(parse_scalar(path, line_number, raw_value)?)
        };
        fields.insert(key.to_owned(), value);
        index += 1;
    }
    Ok(fields)
}

fn parse_inline_list(
    path: &Path,
    line: usize,
    value: &str,
) -> Result<Vec<String>, ExtensionDiscoveryError> {
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return invalid_frontmatter(path, line, "unterminated inline list");
    };
    let mut items = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let chars: Vec<char> = inner.chars().collect();
    for (index, character) in chars.iter().copied().enumerate() {
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(active), current) if active == current => quote = None,
            (None, ',') => {
                let item: String = chars[start..index].iter().collect();
                if !item.trim().is_empty() {
                    items.push(parse_scalar(path, line, item.trim())?);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return invalid_frontmatter(path, line, "unterminated quoted scalar");
    }
    let item: String = chars[start..].iter().collect();
    if !item.trim().is_empty() {
        items.push(parse_scalar(path, line, item.trim())?);
    }
    Ok(items)
}

fn parse_scalar(path: &Path, line: usize, value: &str) -> Result<String, ExtensionDiscoveryError> {
    if let Some(quoted) = value.strip_prefix('"') {
        if !value.ends_with('"') || value.len() < 2 {
            return invalid_frontmatter(path, line, "unterminated double-quoted scalar");
        }
        let json = format!("\"{}\"", &quoted[..quoted.len() - 1]);
        return serde_json::from_str(&json).map_err(|_| {
            ExtensionDiscoveryError::InvalidFrontmatter {
                path: path.to_owned(),
                line,
                message: "invalid double-quoted scalar".to_owned(),
            }
        });
    }
    if let Some(quoted) = value.strip_prefix('\'') {
        if !value.ends_with('\'') || value.len() < 2 {
            return invalid_frontmatter(path, line, "unterminated single-quoted scalar");
        }
        return Ok(quoted[..quoted.len() - 1].replace("''", "'"));
    }
    Ok(value.trim().to_owned())
}

fn parse_template(path: &Path, body: &str) -> Result<CommandTemplate, ExtensionDiscoveryError> {
    let mut parts = Vec::new();
    let mut text_start = 0;
    let mut cursor = 0;
    while cursor < body.len() {
        let remainder = &body[cursor..];
        let parsed = if remainder.starts_with("$ARGUMENTS") {
            Some(("$ARGUMENTS".len(), TemplatePart::Arguments))
        } else if let Some(after_dollar) = remainder.strip_prefix('$') {
            let digits = after_dollar.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                let position = after_dollar[..digits].parse::<usize>().ok();
                position
                    .filter(|position| *position > 0)
                    .map(|position| (digits + 1, TemplatePart::PositionalArgument(position)))
            } else {
                None
            }
        } else if let Some(command) = remainder.strip_prefix("!`") {
            let Some(end) = command.find('`') else {
                return Err(ExtensionDiscoveryError::UnterminatedShellInterpolation {
                    path: path.to_owned(),
                });
            };
            Some((
                end + 3,
                TemplatePart::ShellInterpolation {
                    command: command[..end].to_owned(),
                },
            ))
        } else if remainder.starts_with('@') && is_token_boundary(body, cursor) {
            let candidate_length = remainder[1..]
                .char_indices()
                .take_while(|(_, character)| is_file_reference_character(*character))
                .last()
                .map_or(0, |(index, character)| index + character.len_utf8());
            let candidate = remainder.get(1..=candidate_length).unwrap_or_default();
            let path_value = candidate.trim_end_matches('.');
            let length = path_value.len();
            (length > 0).then(|| {
                (
                    length + 1,
                    TemplatePart::FileInclusion {
                        path: path_value.to_owned(),
                    },
                )
            })
        } else {
            None
        };
        if let Some((consumed, part)) = parsed {
            push_text(&mut parts, &body[text_start..cursor]);
            parts.push(part);
            cursor += consumed;
            text_start = cursor;
        } else {
            let Some(character) = remainder.chars().next() else {
                break;
            };
            cursor += character.len_utf8();
        }
    }
    push_text(&mut parts, &body[text_start..]);
    Ok(CommandTemplate { parts })
}

fn push_text(parts: &mut Vec<TemplatePart>, text: &str) {
    if !text.is_empty() {
        parts.push(TemplatePart::Text(text.to_owned()));
    }
}

fn is_token_boundary(body: &str, cursor: usize) -> bool {
    cursor == 0
        || body[..cursor]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_whitespace() || "([{<\"'=:".contains(character))
}

fn is_file_reference_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '.' | '_' | '-' | '/' | '\\')
}

fn required_scalar(
    path: &Path,
    fields: &BTreeMap<String, FrontmatterValue>,
    field: &'static str,
) -> Result<String, ExtensionDiscoveryError> {
    optional_scalar(path, fields, field)?.ok_or_else(|| ExtensionDiscoveryError::MissingField {
        path: path.to_owned(),
        field,
    })
}

fn optional_scalar(
    path: &Path,
    fields: &BTreeMap<String, FrontmatterValue>,
    field: &'static str,
) -> Result<Option<String>, ExtensionDiscoveryError> {
    match fields.get(field) {
        None => Ok(None),
        Some(FrontmatterValue::Scalar(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(ExtensionDiscoveryError::InvalidFrontmatter {
            path: path.to_owned(),
            line: 1,
            message: format!("`{field}` must be a non-empty scalar"),
        }),
    }
}

fn optional_list(fields: &BTreeMap<String, FrontmatterValue>, field: &'static str) -> Vec<String> {
    match fields.get(field) {
        None => Vec::new(),
        Some(FrontmatterValue::List(values)) => deduplicate(values.clone()),
        Some(FrontmatterValue::Scalar(value)) => deduplicate(
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
    }
}

fn deduplicate(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn invalid_frontmatter<T>(
    path: &Path,
    line: usize,
    message: &str,
) -> Result<T, ExtensionDiscoveryError> {
    Err(ExtensionDiscoveryError::InvalidFrontmatter {
        path: path.to_owned(),
        line,
        message: message.to_owned(),
    })
}

fn valid_frontmatter_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_artifact_name(path: &Path, name: &str) -> Result<(), ExtensionDiscoveryError> {
    let valid = !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        });
    if valid {
        Ok(())
    } else {
        Err(ExtensionDiscoveryError::InvalidName {
            path: path.to_owned(),
            name: name.to_owned(),
        })
    }
}

fn regular_children_with_extension(
    directory: &Path,
    extension: &str,
) -> Result<Vec<PathBuf>, ExtensionDiscoveryError> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    ensure_directory(directory)?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).map_err(|source| ExtensionDiscoveryError::Io {
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| ExtensionDiscoveryError::Io {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| ExtensionDiscoveryError::Io {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionDiscoveryError::UnsafeEntry { path });
        }
        if metadata.is_file() && path.extension().is_some_and(|value| value == extension) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn skill_manifests(directory: &Path) -> Result<Vec<PathBuf>, ExtensionDiscoveryError> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    ensure_directory(directory)?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).map_err(|source| ExtensionDiscoveryError::Io {
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| ExtensionDiscoveryError::Io {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| ExtensionDiscoveryError::Io {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionDiscoveryError::UnsafeEntry { path });
        }
        if !metadata.is_dir() {
            continue;
        }
        let manifest = path.join("SKILL.md");
        if manifest.exists() {
            ensure_regular_file(&manifest)?;
            paths.push(manifest);
        }
    }
    paths.sort();
    Ok(paths)
}

fn collect_resource_paths(
    root: &Path,
    directory: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<(), ExtensionDiscoveryError> {
    ensure_directory(directory)?;
    for entry in fs::read_dir(directory).map_err(|source| ExtensionDiscoveryError::Io {
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| ExtensionDiscoveryError::Io {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| ExtensionDiscoveryError::Io {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionDiscoveryError::UnsafeEntry { path });
        }
        if metadata.is_dir() {
            collect_resource_paths(root, &path, paths)?;
        } else if metadata.is_file() {
            paths.push(
                path.strip_prefix(root)
                    .map_err(|_| ExtensionDiscoveryError::InvalidResourcePath {
                        path: path.clone(),
                    })?
                    .to_owned(),
            );
        } else {
            return Err(ExtensionDiscoveryError::UnsafeEntry { path });
        }
    }
    Ok(())
}

fn validate_relative_resource(path: &Path) -> Result<(), ExtensionDiscoveryError> {
    let valid = !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if valid {
        Ok(())
    } else {
        Err(ExtensionDiscoveryError::InvalidResourcePath {
            path: path.to_owned(),
        })
    }
}

fn ensure_directory(path: &Path) -> Result<(), ExtensionDiscoveryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ExtensionDiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(ExtensionDiscoveryError::UnsafeEntry {
            path: path.to_owned(),
        })
    }
}

fn ensure_regular_file(path: &Path) -> Result<fs::Metadata, ExtensionDiscoveryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ExtensionDiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        Ok(metadata)
    } else {
        Err(ExtensionDiscoveryError::UnsafeEntry {
            path: path.to_owned(),
        })
    }
}

fn read_bounded_utf8(path: &Path, limit: u64) -> Result<String, ExtensionDiscoveryError> {
    let bytes = read_bounded_regular_file(path, limit)?;
    String::from_utf8(bytes).map_err(|_| ExtensionDiscoveryError::NotUtf8 {
        path: path.to_owned(),
    })
}

fn read_bounded_regular_file(path: &Path, limit: u64) -> Result<Vec<u8>, ExtensionDiscoveryError> {
    let metadata = ensure_regular_file(path)?;
    if metadata.len() > limit {
        return Err(ExtensionDiscoveryError::TooLarge {
            path: path.to_owned(),
            limit,
        });
    }
    let bytes = fs::read(path).map_err(|source| ExtensionDiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(ExtensionDiscoveryError::TooLarge {
            path: path.to_owned(),
            limit,
        });
    }
    Ok(bytes)
}

fn read_bounded_relative_file(
    root: &Path,
    relative: &Path,
    limit: u64,
) -> Result<Vec<u8>, ExtensionDiscoveryError> {
    validate_relative_resource(relative)?;
    #[cfg(unix)]
    {
        use std::io::Read;

        let mut directory = rustix::fs::open(
            root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| ExtensionDiscoveryError::Io {
            path: root.to_owned(),
            source: source.into(),
        })?;
        let components = relative.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                return Err(ExtensionDiscoveryError::InvalidResourcePath {
                    path: relative.to_owned(),
                });
            };
            let final_component = index.saturating_add(1) == components.len();
            let mut flags = rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC;
            if !final_component {
                flags |= rustix::fs::OFlags::DIRECTORY;
            }
            let opened = rustix::fs::openat(&directory, *name, flags, rustix::fs::Mode::empty())
                .map_err(|source| ExtensionDiscoveryError::Io {
                    path: root.join(relative),
                    source: source.into(),
                })?;
            if final_component {
                let file = fs::File::from(opened);
                let metadata = file
                    .metadata()
                    .map_err(|source| ExtensionDiscoveryError::Io {
                        path: root.join(relative),
                        source,
                    })?;
                if !metadata.is_file() {
                    return Err(ExtensionDiscoveryError::UnsafeEntry {
                        path: root.join(relative),
                    });
                }
                if metadata.len() > limit {
                    return Err(ExtensionDiscoveryError::TooLarge {
                        path: root.join(relative),
                        limit,
                    });
                }
                let take_limit = limit.saturating_add(1);
                let mut bytes = Vec::new();
                file.take(take_limit)
                    .read_to_end(&mut bytes)
                    .map_err(|source| ExtensionDiscoveryError::Io {
                        path: root.join(relative),
                        source,
                    })?;
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
                    return Err(ExtensionDiscoveryError::TooLarge {
                        path: root.join(relative),
                        limit,
                    });
                }
                return Ok(bytes);
            }
            directory = opened;
        }
        Err(ExtensionDiscoveryError::InvalidResourcePath {
            path: relative.to_owned(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = limit;
        Err(ExtensionDiscoveryError::UnsafeEntry {
            path: root.join(relative),
        })
    }
}

fn file_stem(path: &Path) -> Result<String, ExtensionDiscoveryError> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| ExtensionDiscoveryError::InvalidPath {
            path: path.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;

    use tempfile::TempDir;

    use super::{
        ArtifactKind, ArtifactLocation, ArtifactScope, ExtensionCatalog, ExtensionDiscoveryConfig,
        ExtensionDiscoveryError, TemplatePart,
    };
    use crate::{HookEvent, HookFailurePolicy};

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(path, contents).expect("write fixture");
    }

    use std::path::Path;

    #[test]
    fn trusted_discovery_follows_adr_014_and_is_sorted() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let command = |description: &str| format!("---\ndescription: {description}\n---\nbody");
        write(
            &project.join(".agents/commands/shared.md"),
            &command("project agents"),
        );
        write(
            &project.join(".rottweiler/commands/shared.md"),
            &command("project rottweiler"),
        );
        write(
            &home.join(".agents/commands/shared.md"),
            &command("user agents"),
        );
        write(
            &home.join(".rottweiler/commands/shared.md"),
            &command("user rottweiler"),
        );
        write(&home.join(".agents/commands/zeta.md"), &command("zeta"));
        write(&home.join(".agents/commands/alpha.md"), &command("alpha"));
        write(
            &project.join(".rottweiler/commands/project-over-user.md"),
            &command("project rottweiler"),
        );
        write(
            &home.join(".agents/commands/project-over-user.md"),
            &command("user agents"),
        );
        write(
            &home.join(".agents/commands/user-open-first.md"),
            &command("user agents"),
        );
        write(
            &home.join(".rottweiler/commands/user-open-first.md"),
            &command("user rottweiler"),
        );

        let catalog = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
        )
        .expect("discover");

        let shared = catalog.command("shared").expect("shared");
        assert_eq!(shared.description(), "project agents");
        assert_eq!(shared.origin().scope(), ArtifactScope::Project);
        assert_eq!(shared.origin().location(), ArtifactLocation::Agents);
        assert_eq!(
            catalog
                .command("project-over-user")
                .expect("project precedence")
                .description(),
            "project rottweiler"
        );
        assert_eq!(
            catalog
                .command("user-open-first")
                .expect("user location precedence")
                .description(),
            "user agents"
        );
        assert_eq!(
            catalog
                .commands()
                .map(super::DiscoveredCommand::name)
                .collect::<Vec<_>>(),
            vec![
                "alpha",
                "project-over-user",
                "shared",
                "user-open-first",
                "zeta"
            ]
        );
    }

    #[test]
    fn skills_also_use_first_match_by_declared_name() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write(
            &project.join(".agents/skills/project-dir/SKILL.md"),
            "---\nname: shared-skill\ndescription: project\n---\nproject body",
        );
        write(
            &home.join(".agents/skills/user-dir/SKILL.md"),
            "---\nname: shared-skill\ndescription: user\n---\nuser body",
        );

        let trusted = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
        )
        .expect("trusted discovery");
        assert_eq!(
            trusted
                .skill("shared-skill")
                .expect("project skill")
                .description(),
            "project"
        );

        let untrusted = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("untrusted discovery");
        assert_eq!(
            untrusted
                .skill("shared-skill")
                .expect("user fallback skill")
                .description(),
            "user"
        );
    }

    #[test]
    fn untrusted_project_is_inert_and_does_not_shadow_user_command() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write(
            &project.join(".agents/commands/build.md"),
            "not even valid frontmatter !`touch should-not-run`",
        );
        write(
            &project.join(".rottweiler/skills/audit/SKILL.md"),
            "untrusted and deliberately malformed",
        );
        write(
            &home.join(".agents/commands/build.md"),
            "---\ndescription: safe user command\n---\nuser body",
        );

        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("untrusted source is only inventoried");

        assert_eq!(
            catalog
                .command("build")
                .expect("user fallback")
                .description(),
            "safe user command"
        );
        assert_eq!(catalog.inert_project_artifacts().len(), 2);
        let command = catalog
            .inert_project_artifacts()
            .iter()
            .find(|artifact| artifact.kind() == ArtifactKind::Command)
            .expect("inert command");
        assert!(command.contains_shell_interpolation());
        assert_eq!(command.name(), "build");
    }

    #[test]
    fn command_frontmatter_and_template_operations_remain_lazy() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let path = home.join(".agents/commands/review.md");
        write(
            &path,
            "---\n\
             description: Review a change\n\
             model: fast\n\
             allowed-tools: [Read, 'Bash(git status)']\n\
             argument-hint: '[path] [focus]'\n\
             ---\n\
             Review $ARGUMENTS, first=$1 second=$2. !`git status` Include @src/main.rs.",
        );
        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("discover");
        let command = catalog.command("/review").expect("review command");
        assert_eq!(command.model(), Some("fast"));
        assert_eq!(command.allowed_tools(), ["Read", "Bash(git status)"]);
        assert_eq!(command.argument_hint(), Some("[path] [focus]"));
        assert!(!command.used_legacy_args_alias());

        let template = command.load_template().expect("parse lazy template");
        assert!(template.requires_shell());
        assert!(template.parts().contains(&TemplatePart::Arguments));
        assert!(
            template
                .parts()
                .contains(&TemplatePart::PositionalArgument(1))
        );
        assert!(template.parts().contains(&TemplatePart::FileInclusion {
            path: "src/main.rs".to_owned()
        }));
        assert!(
            template
                .parts()
                .contains(&TemplatePart::ShellInterpolation {
                    command: "git status".to_owned()
                })
        );
    }

    #[test]
    fn legacy_args_alias_is_accepted_but_canonical_key_wins() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write(
            &home.join(".agents/commands/old.md"),
            "---\ndescription: old\nargs: FILE\n---\n$ARGUMENTS",
        );
        write(
            &home.join(".agents/commands/both.md"),
            "---\ndescription: both\nargs: OLD\nargument-hint: NEW\n---\n$ARGUMENTS",
        );
        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("discover");
        assert_eq!(
            catalog.command("old").expect("old").argument_hint(),
            Some("FILE")
        );
        assert!(
            catalog
                .command("old")
                .expect("old")
                .used_legacy_args_alias()
        );
        assert_eq!(
            catalog.command("both").expect("both").argument_hint(),
            Some("NEW")
        );
        assert!(
            !catalog
                .command("both")
                .expect("both")
                .used_legacy_args_alias()
        );
    }

    #[test]
    fn skill_metadata_body_and_resources_are_lazy() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let root = home.join(".agents/skills/release");
        write(
            &root.join("SKILL.md"),
            "---\nname: release\ndescription: Prepare a release\nallowed-tools:\n  - Read\n  - Bash(cargo test)\n---\nRelease instructions.",
        );
        write(&root.join("scripts/check.sh"), "#!/bin/sh\nexit 0\n");
        write(&root.join("references/policy.md"), "policy");

        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("discover");
        let skill = catalog.skill("release").expect("skill");
        assert_eq!(skill.description(), "Prepare a release");
        assert_eq!(skill.allowed_tools(), ["Read", "Bash(cargo test)"]);
        assert_eq!(
            skill.load_instructions().expect("instructions"),
            "Release instructions."
        );
        let resources = skill.resources().expect("resources");
        assert_eq!(
            resources
                .iter()
                .map(super::SkillResource::relative_path)
                .collect::<Vec<_>>(),
            vec![
                Path::new("references/policy.md"),
                Path::new("scripts/check.sh")
            ]
        );
        assert_eq!(
            resources[0].load().expect("load resource").bytes(),
            b"policy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_resource_load_fails_closed_after_directory_symlink_swap() {
        use std::os::unix::fs::symlink;

        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let root = home.join(".agents/skills/release");
        write(
            &root.join("SKILL.md"),
            "---\nname: release\ndescription: Prepare\n---\nInstructions",
        );
        write(&root.join("references/policy.md"), "trusted policy");
        let outside = fixture.path().join("outside");
        write(&outside.join("policy.md"), "swapped policy");

        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("discover");
        let resource = catalog
            .skill("release")
            .expect("skill")
            .resources()
            .expect("resources")
            .into_iter()
            .find(|resource| resource.relative_path() == Path::new("references/policy.md"))
            .expect("policy resource");
        fs::rename(root.join("references"), root.join("references.original"))
            .expect("move original directory");
        symlink(&outside, root.join("references")).expect("swap directory symlink");

        assert!(resource.load().is_err());
    }

    #[test]
    fn changed_markdown_fails_closed_before_lazy_load() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let path = home.join(".agents/commands/check.md");
        write(&path, "---\ndescription: check\n---\noriginal");
        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("discover");
        fs::write(&path, "---\ndescription: check\n---\nchanged").expect("mutate");

        assert!(matches!(
            catalog.command("check").expect("check").load_template(),
            Err(ExtensionDiscoveryError::ChangedAfterDiscovery { .. })
        ));
    }

    #[test]
    fn malformed_active_frontmatter_and_unclosed_shell_are_rejected() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write(
            &home.join(".agents/commands/missing.md"),
            "---\nmodel: fast\n---\nbody",
        );
        assert!(matches!(
            ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home)),
            Err(ExtensionDiscoveryError::MissingField {
                field: "description",
                ..
            })
        ));

        fs::remove_file(home.join(".agents/commands/missing.md")).expect("remove malformed");
        write(
            &home.join(".agents/commands/shell.md"),
            "---\ndescription: shell\n---\n!`unterminated",
        );
        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("metadata discovery is lazy");
        assert!(matches!(
            catalog.command("shell").expect("shell").load_template(),
            Err(ExtensionDiscoveryError::UnterminatedShellInterpolation { .. })
        ));
    }

    #[test]
    fn declarative_hooks_parse_defaults_options_and_dispatch_order_without_execution() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let marker = fixture.path().join("must-not-exist");
        write(
            &home.join(".agents/hooks.toml"),
            &format!(
                "[[hook]]\n\
                 id = \"late\"\n\
                 event = \"post_tool\"\n\
                 matcher = \"edit(*.rs)\"\n\
                 run = \"cargo fmt --check {{file}}\"\n\
                 priority = 10\n\
                 timeout_ms = 250\n\
                 failure-policy = \"fail-closed\"\n\n\
                 [[hook]]\n\
                 event = \"session_start\"\n\
                 matcher = \"*\"\n\
                 run = \"touch {}\"\n\
                 priority = -5\n",
                marker.display()
            ),
        );

        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("discover hooks");
        let hooks = catalog.shell_hooks();
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].id(), "shell.user.agents.2");
        assert_eq!(hooks[0].registration().event(), HookEvent::SessionStart);
        assert_eq!(hooks[0].registration().timeout().as_millis(), 5_000);
        assert_eq!(
            hooks[0].registration().failure_policy(),
            HookFailurePolicy::FailOpen
        );
        assert_eq!(hooks[1].id(), "late");
        assert_eq!(hooks[1].matcher(), "edit(*.rs)");
        assert_eq!(hooks[1].registration().event(), HookEvent::PostTool);
        assert_eq!(hooks[1].registration().priority(), 10);
        assert_eq!(hooks[1].registration().timeout().as_millis(), 250);
        assert_eq!(
            hooks[1].registration().failure_policy(),
            HookFailurePolicy::FailClosed
        );
        assert_eq!(
            hooks[1].load_command().expect("load command"),
            "cargo fmt --check {file}"
        );
        let _command_data = hooks[0].load_command().expect("load opaque command");
        assert!(
            !marker.exists(),
            "discovery and loading must not execute hooks"
        );
    }

    #[test]
    fn hook_ids_follow_adr_precedence_and_untrusted_project_stays_inert() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        write(
            &project.join(".agents/hooks.toml"),
            "[[hook]]\nid = \"shared\"\nevent = \"pre_tool\"\nmatcher = \"bash(*)\"\nrun = \"project-command\"\n",
        );
        write(
            &project.join(".rottweiler/hooks.toml"),
            "[[hook]]\nid = \"shared\"\nevent = \"pre_tool\"\nmatcher = \"bash(*)\"\nrun = \"project-rottweiler-command\"\n",
        );
        write(
            &home.join(".agents/hooks.toml"),
            "[[hook]]\nid = \"shared\"\nevent = \"pre_tool\"\nmatcher = \"bash(*)\"\nrun = \"user-command\"\n",
        );
        write(
            &home.join(".rottweiler/hooks.toml"),
            "[[hook]]\nid = \"shared\"\nevent = \"pre_tool\"\nmatcher = \"bash(*)\"\nrun = \"user-rottweiler-command\"\n",
        );

        let trusted = ExtensionCatalog::discover(
            &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
        )
        .expect("trusted hooks");
        assert_eq!(
            trusted
                .shell_hook("shared")
                .expect("project hook")
                .load_command()
                .expect("command"),
            "project-command"
        );

        let untrusted = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("untrusted project hooks stay unparsed");
        assert_eq!(
            untrusted
                .shell_hook("shared")
                .expect("user hook")
                .load_command()
                .expect("command"),
            "user-command"
        );
        let inert_hooks = untrusted
            .inert_project_artifacts()
            .iter()
            .filter(|artifact| artifact.kind() == ArtifactKind::Hook)
            .collect::<Vec<_>>();
        assert_eq!(inert_hooks.len(), 2);
        assert!(
            inert_hooks
                .iter()
                .all(|artifact| artifact.executes_command())
        );
        assert!(
            inert_hooks
                .iter()
                .all(|artifact| !artifact.contains_shell_interpolation())
        );
    }

    #[test]
    fn hooks_toml_mutation_fails_closed_before_command_load() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let path = home.join(".agents/hooks.toml");
        write(
            &path,
            "[[hook]]\nid = \"check\"\nevent = \"turn_end\"\nmatcher = \"*\"\nrun = \"original\"\n",
        );
        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home))
            .expect("discover");
        fs::write(
            &path,
            "[[hook]]\nid = \"check\"\nevent = \"turn_end\"\nmatcher = \"*\"\nrun = \"changed\"\n",
        )
        .expect("mutate");

        assert!(matches!(
            catalog.shell_hook("check").expect("hook").load_command(),
            Err(ExtensionDiscoveryError::ChangedAfterDiscovery { .. })
        ));
    }

    #[test]
    fn invalid_hook_schema_event_and_multiline_commands_are_rejected() {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let path = home.join(".agents/hooks.toml");
        write(
            &path,
            "[[hook]]\nevent = \"not_real\"\nmatcher = \"*\"\nrun = \"echo ok\"\n",
        );
        assert!(matches!(
            ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home)),
            Err(ExtensionDiscoveryError::InvalidHook { index: 1, .. })
        ));

        write(
            &path,
            "[[hook]]\nevent = \"post_tool\"\nmatcher = \"*\"\nrun = \"first\\nsecond\"\n",
        );
        assert!(matches!(
            ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home)),
            Err(ExtensionDiscoveryError::InvalidHook { index: 1, .. })
        ));
    }
}
