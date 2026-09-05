//! Declarative command and skill discovery.
//!
//! Discovery reads metadata, but it never evaluates command templates or
//! project content. Shell interpolation and file inclusion remain typed lazy
//! template parts for the engine permission/trust layers to resolve later.

mod filesystem;
pub(crate) use filesystem::read_bounded_relative_utf8;
use filesystem::{
    ScanDiagnostic, collect_resource_paths, read_bounded_relative_file, read_bounded_utf8,
    regular_children_with_extension, skill_manifests, strict_regular_children_with_extension,
    strict_skill_manifests, validate_relative_resource,
};

mod markdown;
use markdown::{
    discover_agent, discover_command, discover_skill, parse_frontmatter, parse_template,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
};

use rw_tools::validate_mcp_virtual_tool;
use rw_types::SessionMode;
use thiserror::Error;

use crate::{CommandDescriptor, DiscoveredWorkflow, ModeDefinition};

mod shell_hook;

pub use shell_hook::DiscoveredShellHook;

pub(crate) const MAX_MARKDOWN_BYTES: u64 = 1024 * 1024;
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
    pub(crate) fn new(scope: ArtifactScope, location: ArtifactLocation, path: PathBuf) -> Self {
        Self {
            scope,
            location,
            path,
        }
    }

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
    Agent,
    Workflow,
    Mode,
}

/// One extension source or incomplete inert inventory refused during discovery.
///
/// Diagnostics are presentation-neutral and bounded so the runtime can safely
/// forward them to logs, doctor, and engine clients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionDiagnostic {
    path: PathBuf,
    scope: ArtifactScope,
    location: ArtifactLocation,
    kind: ArtifactKind,
    message: String,
    artifact_name: Option<String>,
}

/// An untrusted project whose extension inventory was discarded as incomplete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UninventoriedProjectRoot {
    root: PathBuf,
    offending_path: PathBuf,
    message: String,
}

impl UninventoriedProjectRoot {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn offending_path(&self) -> &Path {
        &self.offending_path
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl ExtensionDiagnostic {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn scope(&self) -> ArtifactScope {
        self.scope
    }

    #[must_use]
    pub const fn location(&self) -> ArtifactLocation {
        self.location
    }

    #[must_use]
    pub const fn kind(&self) -> ArtifactKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
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
    root: PathBuf,
    relative: PathBuf,
    digest: blake3::Hash,
}

impl LazyMarkdownBody {
    fn load(&self) -> Result<String, ExtensionDiscoveryError> {
        let bytes = read_bounded_relative_file(&self.root, &self.relative, MAX_MARKDOWN_BYTES)?;
        let contents = String::from_utf8(bytes).map_err(|_| ExtensionDiscoveryError::NotUtf8 {
            path: self.path.clone(),
        })?;
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

/// A lazily loaded declarative subagent definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredAgent {
    name: String,
    description: String,
    model: String,
    tools: Vec<String>,
    permission_mode: SessionMode,
    max_turns: usize,
    origin: ArtifactOrigin,
    body: LazyMarkdownBody,
}

impl DiscoveredAgent {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    #[must_use]
    pub const fn permission_mode(&self) -> SessionMode {
        self.permission_mode
    }

    #[must_use]
    pub const fn max_turns(&self) -> usize {
        self.max_turns
    }

    #[must_use]
    pub const fn origin(&self) -> &ArtifactOrigin {
        &self.origin
    }

    /// Loads the system prompt only when this agent is selected.
    ///
    /// # Errors
    ///
    /// Fails closed if the definition changed after discovery.
    pub fn load_system_prompt(&self) -> Result<String, ExtensionDiscoveryError> {
        self.body.load()
    }
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
    agents: BTreeMap<String, DiscoveredAgent>,
    workflows: BTreeMap<String, DiscoveredWorkflow>,
    modes: BTreeMap<String, ModeDefinition>,
    shell_hooks: Vec<DiscoveredShellHook>,
    inert_project_artifacts: Vec<InertProjectArtifact>,
    uninventoried_project_roots: Vec<UninventoriedProjectRoot>,
    diagnostics: Vec<ExtensionDiagnostic>,
}

impl ExtensionCatalog {
    /// Discovers commands and skills in ADR-014 order. First active match by
    /// name wins: project `.agents`, project `.rottweiler`, user `.agents`,
    /// user `.rottweiler`. An untrusted project is inventoried but skipped for
    /// active resolution.
    ///
    /// Malformed, unsafe, or unreadable sources are skipped and reported
    /// through [`Self::diagnostics`]. An incomplete untrusted-project inventory
    /// is discarded as a unit and recorded in
    /// [`Self::uninventoried_project_roots`].
    #[must_use]
    pub fn discover(config: &ExtensionDiscoveryConfig) -> Self {
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
                    catalog.discover_active_root(ArtifactScope::Project, location, &root);
                }
            } else {
                catalog.inventory_inert_project(project_root, &project_sources);
            }
        }
        for (location, root) in user_sources {
            catalog.discover_active_root(ArtifactScope::User, location, &root);
        }
        catalog.shell_hooks.sort_by(|left, right| {
            left.registration()
                .priority()
                .cmp(&right.registration().priority())
                .then_with(|| left.id().cmp(right.id()))
        });
        catalog
            .diagnostics
            .sort_by(|left, right| left.path.cmp(&right.path));
        catalog
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

    #[must_use]
    pub fn agent(&self, name: &str) -> Option<&DiscoveredAgent> {
        self.agents.get(name)
    }

    /// Active agents in stable name order.
    #[must_use]
    pub fn agents(&self) -> impl ExactSizeIterator<Item = &DiscoveredAgent> {
        self.agents.values()
    }

    #[must_use]
    pub fn workflow(&self, name: &str) -> Option<&DiscoveredWorkflow> {
        self.workflows.get(name)
    }

    /// Active workflows in stable name order.
    #[must_use]
    pub fn workflows(&self) -> impl ExactSizeIterator<Item = &DiscoveredWorkflow> {
        self.workflows.values()
    }

    #[must_use]
    pub fn mode(&self, id: &str) -> Option<&ModeDefinition> {
        self.modes.get(id)
    }

    /// Active declarative modes in stable id order.
    #[must_use]
    pub fn modes(&self) -> impl ExactSizeIterator<Item = &ModeDefinition> {
        self.modes.values()
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

    /// Untrusted project roots whose partial inventories were discarded.
    #[must_use]
    pub fn uninventoried_project_roots(&self) -> &[UninventoriedProjectRoot] {
        &self.uninventoried_project_roots
    }

    /// Refused artifacts and incomplete inventories in stable path order.
    #[must_use]
    pub fn diagnostics(&self) -> &[ExtensionDiagnostic] {
        &self.diagnostics
    }

    fn discover_active_root(
        &mut self,
        scope: ArtifactScope,
        location: ArtifactLocation,
        root: &Path,
    ) {
        self.discover_commands(scope, location, root);
        self.discover_skills(scope, location, root);
        self.discover_agents(scope, location, root);
        self.discover_workflows(scope, location, root);
        self.discover_modes(scope, location, root);
        self.discover_hooks(scope, location, root);
    }

    fn discover_commands(&mut self, scope: ArtifactScope, location: ArtifactLocation, root: &Path) {
        let commands = regular_children_with_extension(&root.join("commands"), "md");
        self.record_scan_diagnostics(scope, location, ArtifactKind::Command, commands.diagnostics);
        for path in commands.paths {
            match discover_command(scope, location, root, &path) {
                Ok(command) => {
                    let name = command.name.clone();
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        self.commands.entry(name.clone())
                    {
                        mark_lower_precedence_fallback(
                            &mut self.diagnostics,
                            ArtifactKind::Command,
                            &name,
                            &path,
                        );
                        entry.insert(command);
                    }
                }
                Err(error) => {
                    self.record_diagnostic(scope, location, ArtifactKind::Command, path, &error);
                }
            }
        }
    }

    fn discover_skills(&mut self, scope: ArtifactScope, location: ArtifactLocation, root: &Path) {
        let skills = skill_manifests(&root.join("skills"));
        self.record_scan_diagnostics(scope, location, ArtifactKind::Skill, skills.diagnostics);
        for path in skills.paths {
            match discover_skill(scope, location, root, &path) {
                Ok(skill) => {
                    let name = skill.name.clone();
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        self.skills.entry(name.clone())
                    {
                        mark_lower_precedence_fallback(
                            &mut self.diagnostics,
                            ArtifactKind::Skill,
                            &name,
                            &path,
                        );
                        entry.insert(skill);
                    }
                }
                Err(error) => {
                    self.record_diagnostic(scope, location, ArtifactKind::Skill, path, &error);
                }
            }
        }
    }

    fn discover_agents(&mut self, scope: ArtifactScope, location: ArtifactLocation, root: &Path) {
        let agents = regular_children_with_extension(&root.join("agents"), "md");
        self.record_scan_diagnostics(scope, location, ArtifactKind::Agent, agents.diagnostics);
        for path in agents.paths {
            match discover_agent(scope, location, root, &path) {
                Ok(agent) => {
                    let name = agent.name.clone();
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        self.agents.entry(name.clone())
                    {
                        mark_lower_precedence_fallback(
                            &mut self.diagnostics,
                            ArtifactKind::Agent,
                            &name,
                            &path,
                        );
                        entry.insert(agent);
                    }
                }
                Err(error) => {
                    self.record_diagnostic(scope, location, ArtifactKind::Agent, path, &error);
                }
            }
        }
    }

    fn discover_workflows(
        &mut self,
        scope: ArtifactScope,
        location: ArtifactLocation,
        root: &Path,
    ) {
        let workflows = regular_children_with_extension(&root.join("workflows"), "toml");
        self.record_scan_diagnostics(
            scope,
            location,
            ArtifactKind::Workflow,
            workflows.diagnostics,
        );
        for path in workflows.paths {
            match crate::workflow::discover_workflow(scope, location, root, &path) {
                Ok(workflow) => {
                    let name = workflow.name().to_owned();
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        self.workflows.entry(name.clone())
                    {
                        mark_lower_precedence_fallback(
                            &mut self.diagnostics,
                            ArtifactKind::Workflow,
                            &name,
                            &path,
                        );
                        entry.insert(workflow);
                    }
                }
                Err(error) => {
                    self.record_diagnostic(scope, location, ArtifactKind::Workflow, path, &error);
                }
            }
        }
    }

    fn discover_modes(&mut self, scope: ArtifactScope, location: ArtifactLocation, root: &Path) {
        let modes = regular_children_with_extension(&root.join("modes"), "toml");
        self.record_scan_diagnostics(scope, location, ArtifactKind::Mode, modes.diagnostics);
        for path in modes.paths {
            let origin = ArtifactOrigin::new(scope, location, path.clone());
            match crate::mode::parse_mode_file(root, &path, origin) {
                Ok(mode) => {
                    let name = mode.id().0.clone();
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        self.modes.entry(name.clone())
                    {
                        mark_lower_precedence_fallback(
                            &mut self.diagnostics,
                            ArtifactKind::Mode,
                            &name,
                            &path,
                        );
                        entry.insert(mode);
                    }
                }
                Err(error) => {
                    self.record_diagnostic(scope, location, ArtifactKind::Mode, path, &error);
                }
            }
        }
    }

    fn discover_hooks(&mut self, scope: ArtifactScope, location: ArtifactLocation, root: &Path) {
        let hooks_path = root.join("hooks.toml");
        match fs::symlink_metadata(&hooks_path) {
            Ok(_) => match shell_hook::discover_file(scope, location, &hooks_path) {
                Ok(hooks) => {
                    for hook in hooks {
                        if self
                            .shell_hooks
                            .iter()
                            .all(|existing| existing.id() != hook.id())
                        {
                            self.shell_hooks.push(hook);
                        }
                    }
                }
                Err(error) => {
                    self.record_diagnostic(scope, location, ArtifactKind::Hook, hooks_path, &error);
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => self.record_diagnostic(
                scope,
                location,
                ArtifactKind::Hook,
                hooks_path.clone(),
                &ExtensionDiscoveryError::Io {
                    path: hooks_path,
                    source,
                },
            ),
        }
    }

    fn record_scan_diagnostics(
        &mut self,
        scope: ArtifactScope,
        location: ArtifactLocation,
        kind: ArtifactKind,
        diagnostics: Vec<ScanDiagnostic>,
    ) {
        for diagnostic in diagnostics {
            self.record_diagnostic(scope, location, kind, diagnostic.path, &diagnostic.error);
        }
    }

    fn record_diagnostic(
        &mut self,
        scope: ArtifactScope,
        location: ArtifactLocation,
        kind: ArtifactKind,
        path: PathBuf,
        error: &ExtensionDiscoveryError,
    ) {
        self.diagnostics.push(ExtensionDiagnostic {
            artifact_name: diagnostic_artifact_name(kind, &path),
            path,
            scope,
            location,
            kind,
            message: sanitize_diagnostic_message(&error.to_string()),
        });
    }

    fn inventory_inert_project(
        &mut self,
        project_root: &Path,
        sources: &[(ArtifactLocation, PathBuf); 2],
    ) {
        let mut artifacts = Vec::new();
        for (location, root) in sources {
            if let Err(error) = Self::inventory_inert_project_root(root, &mut artifacts) {
                let offending_path = discovery_error_path(&error).to_owned();
                let kind = inventory_artifact_kind(root, &offending_path);
                self.record_diagnostic(
                    ArtifactScope::Project,
                    *location,
                    kind,
                    offending_path.clone(),
                    &error,
                );
                self.uninventoried_project_roots
                    .push(UninventoriedProjectRoot {
                        root: project_root.to_owned(),
                        offending_path,
                        message: sanitize_diagnostic_message(&error.to_string()),
                    });
                return;
            }
        }
        self.inert_project_artifacts.extend(artifacts);
    }

    fn inventory_inert_project_root(
        root: &Path,
        artifacts: &mut Vec<InertProjectArtifact>,
    ) -> Result<(), ExtensionDiscoveryError> {
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(ExtensionDiscoveryError::UnsafeEntry {
                    path: root.to_owned(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(ExtensionDiscoveryError::Io {
                    path: root.to_owned(),
                    source,
                });
            }
        }
        for path in strict_regular_children_with_extension(&root.join("commands"), "md")? {
            let contains_shell_interpolation = match read_bounded_utf8(&path, MAX_MARKDOWN_BYTES) {
                Ok(contents) => contents.contains("!`"),
                Err(
                    ExtensionDiscoveryError::TooLarge { .. }
                    | ExtensionDiscoveryError::NotUtf8 { .. },
                ) => true,
                Err(error) => return Err(error),
            };
            artifacts.push(InertProjectArtifact {
                kind: ArtifactKind::Command,
                name: inert_file_stem(&path),
                path,
                contains_shell_interpolation,
                executes_command: contains_shell_interpolation,
            });
        }
        for path in strict_skill_manifests(&root.join("skills"))? {
            artifacts.push(InertProjectArtifact {
                kind: ArtifactKind::Skill,
                name: path.parent().and_then(Path::file_name).map_or_else(
                    || path.to_string_lossy().into_owned(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                path,
                contains_shell_interpolation: false,
                executes_command: false,
            });
        }
        for path in strict_regular_children_with_extension(&root.join("agents"), "md")? {
            artifacts.push(InertProjectArtifact {
                kind: ArtifactKind::Agent,
                name: inert_file_stem(&path),
                path,
                contains_shell_interpolation: false,
                executes_command: false,
            });
        }
        for path in strict_regular_children_with_extension(&root.join("workflows"), "toml")? {
            artifacts.push(InertProjectArtifact {
                kind: ArtifactKind::Workflow,
                name: inert_file_stem(&path),
                path,
                contains_shell_interpolation: false,
                executes_command: true,
            });
        }
        for path in strict_regular_children_with_extension(&root.join("modes"), "toml")? {
            artifacts.push(InertProjectArtifact {
                kind: ArtifactKind::Mode,
                name: inert_file_stem(&path),
                path,
                contains_shell_interpolation: false,
                executes_command: false,
            });
        }
        let hooks_path = root.join("hooks.toml");
        match fs::symlink_metadata(&hooks_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                artifacts.push(InertProjectArtifact {
                    kind: ArtifactKind::Hook,
                    name: "hooks".to_owned(),
                    path: hooks_path,
                    contains_shell_interpolation: false,
                    executes_command: true,
                });
            }
            Ok(_) => return Err(ExtensionDiscoveryError::UnsafeEntry { path: hooks_path }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ExtensionDiscoveryError::Io {
                    path: hooks_path,
                    source,
                });
            }
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
    #[error("invalid agent definition `{path}`: {message}")]
    InvalidAgent { path: PathBuf, message: String },
    #[error("invalid workflow `{path}`: {message}")]
    InvalidWorkflow { path: PathBuf, message: String },
    #[error("invalid mode definition `{path}`: {message}")]
    InvalidMode { path: PathBuf, message: String },
}

fn discovery_error_path(error: &ExtensionDiscoveryError) -> &Path {
    match error {
        ExtensionDiscoveryError::Io { path, .. }
        | ExtensionDiscoveryError::UnsafeEntry { path }
        | ExtensionDiscoveryError::TooLarge { path, .. }
        | ExtensionDiscoveryError::NotUtf8 { path }
        | ExtensionDiscoveryError::InvalidPath { path }
        | ExtensionDiscoveryError::MissingFrontmatter { path }
        | ExtensionDiscoveryError::UnterminatedFrontmatter { path }
        | ExtensionDiscoveryError::InvalidFrontmatter { path, .. }
        | ExtensionDiscoveryError::MissingField { path, .. }
        | ExtensionDiscoveryError::InvalidName { path, .. }
        | ExtensionDiscoveryError::ChangedAfterDiscovery { path }
        | ExtensionDiscoveryError::UnterminatedShellInterpolation { path }
        | ExtensionDiscoveryError::InvalidResourcePath { path }
        | ExtensionDiscoveryError::InvalidHooksToml { path, .. }
        | ExtensionDiscoveryError::InvalidHook { path, .. }
        | ExtensionDiscoveryError::InvalidAgent { path, .. }
        | ExtensionDiscoveryError::InvalidWorkflow { path, .. }
        | ExtensionDiscoveryError::InvalidMode { path, .. } => path,
    }
}

fn inventory_artifact_kind(root: &Path, path: &Path) -> ArtifactKind {
    let relative = path.strip_prefix(root).unwrap_or(path);
    match relative.components().next() {
        Some(Component::Normal(name)) if name == "skills" => ArtifactKind::Skill,
        Some(Component::Normal(name)) if name == "agents" => ArtifactKind::Agent,
        Some(Component::Normal(name)) if name == "workflows" => ArtifactKind::Workflow,
        Some(Component::Normal(name)) if name == "modes" => ArtifactKind::Mode,
        Some(Component::Normal(name)) if name == "hooks.toml" => ArtifactKind::Hook,
        _ => ArtifactKind::Command,
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

fn diagnostic_artifact_name(kind: ArtifactKind, path: &Path) -> Option<String> {
    match kind {
        ArtifactKind::Skill | ArtifactKind::Agent => read_bounded_utf8(path, MAX_MARKDOWN_BYTES)
            .ok()
            .and_then(|contents| {
                contents.lines().find_map(|line| {
                    line.strip_prefix("name:")
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                })
            })
            .or_else(|| {
                (kind == ArtifactKind::Skill)
                    .then(|| path.parent()?.file_name()?.to_str().map(str::to_owned))
                    .flatten()
            }),
        ArtifactKind::Command | ArtifactKind::Workflow | ArtifactKind::Mode => {
            path.file_stem()?.to_str().map(str::to_owned)
        }
        ArtifactKind::Hook => None,
    }
}

fn sanitize_diagnostic_message(message: &str) -> String {
    const MAX_DIAGNOSTIC_CHARS: usize = 1_024;
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_DIAGNOSTIC_CHARS)
        .collect()
}

fn mark_lower_precedence_fallback(
    diagnostics: &mut [ExtensionDiagnostic],
    kind: ArtifactKind,
    name: &str,
    selected_path: &Path,
) {
    for diagnostic in diagnostics.iter_mut().filter(|diagnostic| {
        diagnostic.kind == kind && diagnostic.artifact_name.as_deref() == Some(name)
    }) {
        let _ = write!(
            diagnostic.message,
            "; lower-precedence valid artifact `{name}` selected from `{}`",
            selected_path.display()
        );
        diagnostic.message = sanitize_diagnostic_message(&diagnostic.message);
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

fn inert_file_stem(path: &Path) -> String {
    path.file_stem().map_or_else(
        || path.to_string_lossy().into_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests;
