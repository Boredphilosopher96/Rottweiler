use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use crate::trust::{FolderTrustState, FolderTrustStore};

use super::{
    ConfigError, ConfigLoader, ConfigSource, ConfigWarning, FileScope, LoadedConfig,
    apply_environment, apply_file, apply_override, configured_setting_value,
    defaults_with_provenance, nonempty_value, paths_collide, read_assessed_project_file, read_file,
    read_tui_provenance, set_source, validate, warn_ignored_project_sections,
};

impl ConfigLoader {
    /// Discovers paths and captures environment variables from the running process.
    ///
    /// `ROTTWEILER_HOME` wins, followed by an explicitly configured
    /// `XDG_CONFIG_HOME`, with `~/.rottweiler` as the documented fallback.
    ///
    /// # Errors
    ///
    /// Returns an error when the process working directory cannot be read.
    pub fn from_environment() -> Result<Self, ConfigError> {
        let environment = env::vars().collect::<BTreeMap<_, _>>();
        let project_root = env::current_dir().map_err(ConfigError::CurrentDirectory)?;
        Self::from_captured_environment(environment, &project_root)
    }

    pub(super) fn from_captured_environment(
        environment: BTreeMap<String, String>,
        project_root: &Path,
    ) -> Result<Self, ConfigError> {
        let (root_name, user_root) =
            if let Some(root) = nonempty_value(&environment, "ROTTWEILER_HOME") {
                ("ROTTWEILER_HOME", PathBuf::from(root))
            } else if let Some(root) = nonempty_value(&environment, "XDG_CONFIG_HOME") {
                ("XDG_CONFIG_HOME", PathBuf::from(root).join("rottweiler"))
            } else {
                let home = nonempty_value(&environment, "HOME")
                    .ok_or(ConfigError::MissingUserConfigRoot)?;
                ("HOME", PathBuf::from(home).join(".rottweiler"))
            };
        if !user_root.is_absolute() {
            return Err(ConfigError::InvalidUserConfigRoot {
                name: root_name.to_owned(),
                value: user_root.display().to_string(),
            });
        }

        Ok(Self {
            user_path: user_root.join("config.toml"),
            project_path: project_root.join(".rottweiler/config.toml"),
            project_root: project_root.to_owned(),
            trust_store_path: user_root.join("trust.json"),
            project_trust_override: None,
            warn_on_dangerous_override: false,
            environment,
            cli_overrides: Vec::new(),
        })
    }

    /// Creates an isolated loader for tests and embedded SDK callers.
    #[must_use]
    pub fn new(user_path: PathBuf, project_path: PathBuf) -> Self {
        let project_parent = project_path.parent().unwrap_or_else(|| Path::new("."));
        let project_root = if project_parent
            .file_name()
            .is_some_and(|name| name == ".rottweiler")
        {
            project_parent.parent().unwrap_or(project_parent)
        } else {
            project_parent
        }
        .to_path_buf();
        let trust_store_path = user_path.with_file_name("trust.json");
        Self {
            user_path,
            project_path,
            project_root,
            trust_store_path,
            project_trust_override: None,
            warn_on_dangerous_override: false,
            environment: BTreeMap::new(),
            cli_overrides: Vec::new(),
        }
    }

    /// Replaces the captured environment layer.
    #[must_use]
    pub fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = environment;
        self
    }

    /// Adds highest-precedence `key=value` CLI overrides.
    #[must_use]
    pub fn with_cli_overrides(mut self, overrides: Vec<String>) -> Self {
        self.cli_overrides = overrides;
        self
    }

    /// Override the persisted folder-trust decision. `true` is the explicit
    /// `--dangerously-trust` CI escape hatch and does not mutate the ledger.
    #[must_use]
    pub fn with_project_trust(mut self, trusted: bool) -> Self {
        self.project_trust_override = Some(trusted);
        self
    }

    /// Enable project executable configuration for this process without
    /// persisting trust. Intended only for explicit CI images.
    #[must_use]
    pub fn dangerously_trust_project(mut self) -> Self {
        self.project_trust_override = Some(true);
        self.warn_on_dangerous_override = true;
        self
    }

    /// Canonicalization input used for project trust assessment.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// User-private trust ledger adjacent to the user configuration.
    #[must_use]
    pub fn trust_store_path(&self) -> &Path {
        &self.trust_store_path
    }

    /// User-scoped credential fallback adjacent to the effective user config.
    #[must_use]
    pub fn credentials_path(&self) -> PathBuf {
        self.user_path.with_file_name("credentials.toml")
    }

    /// Loads, deep-merges, validates, and annotates all configured layers.
    ///
    /// # Errors
    ///
    /// Returns an error when a file cannot be read, TOML fails the schema,
    /// an override is malformed, or the effective values are invalid.
    pub fn load(&self) -> Result<LoadedConfig, ConfigError> {
        if paths_collide(&self.user_path, &self.project_path) {
            return Err(ConfigError::ScopeCollision(self.user_path.clone()));
        }
        tracing::debug!(
            user_config = %self.user_path.display(),
            project_config = %self.project_path.display(),
            "loading configuration"
        );
        let mut loaded = defaults_with_provenance();
        if let Some(file) = read_file(&self.user_path)? {
            let source = ConfigSource::UserFile(self.user_path.clone());
            apply_file(&mut loaded, file, &source, FileScope::User);
            for (key, value) in read_tui_provenance(&self.user_path)? {
                if configured_setting_value(&loaded.config, &key).as_deref() == Some(&value)
                    && matches!(loaded.provenance(&key), Some(ConfigSource::UserFile(_)))
                {
                    set_source(
                        &mut loaded,
                        &key,
                        &ConfigSource::UserTui(self.user_path.clone()),
                    );
                }
            }
        }
        let assessment =
            FolderTrustStore::new(self.trust_store_path.clone()).assess(&self.project_root)?;
        let project_trusted = self
            .project_trust_override
            .unwrap_or_else(|| matches!(assessment.state(), FolderTrustState::Trusted));
        loaded.project_trusted = project_trusted;
        if let Some(file) = read_assessed_project_file(&self.project_path, &assessment)? {
            let source = ConfigSource::ProjectFile(self.project_path.clone());
            if project_trusted {
                if self.warn_on_dangerous_override && !assessment.project_execution_enabled() {
                    loaded.warnings.push(ConfigWarning {
                        message: format!(
                            "dangerously trusting executable project configuration from {source} without persisting a folder-trust decision"
                        ),
                    });
                }
                apply_file(&mut loaded, file, &source, FileScope::Project);
            } else {
                warn_ignored_project_sections(&mut loaded, &file, &source);
                loaded.warnings.push(ConfigWarning {
                    message: format!(
                        "ignored untrusted project configuration from {source}; review the executable inventory with `rw trust status` before granting trust"
                    ),
                });
            }
        }
        apply_environment(&mut loaded, &self.environment)?;
        for cli_override in &self.cli_overrides {
            apply_override(&mut loaded, cli_override, &ConfigSource::Cli)?;
        }
        validate(&loaded.config)?;
        Ok(loaded)
    }
}
