use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex};

use rw_types::McpServerId;
use rw_types::config::Config;
use url::Url;

use super::{
    ConfigError, ConfigLoader, ConfigSource, LoadedConfig, MAX_MCP_ARG_BYTES, MAX_MCP_ARGV_ENTRIES,
    MAX_MCP_ENVIRONMENT_ENTRIES, MAX_MCP_ENVIRONMENT_NAME_BYTES, MAX_MCP_ENVIRONMENT_VALUE_BYTES,
    MAX_TUI_AUX_CONFIG_BYTES, TUI_SETTING_PROCESS_LOCKS, TUI_SETTING_TEMP_SEQUENCE,
    parse_tui_budget_cap, parse_tui_budget_warning, parse_tui_token_limit, validate_tui_setting,
};

impl ConfigLoader {
    /// Persists one allowlisted user-scoped setting through an atomic private rewrite.
    ///
    /// The project file is never a write target, including for security-sensitive
    /// permission defaults. The complete resulting configuration is reloaded and
    /// validated before success is returned.
    ///
    /// # Errors
    ///
    /// Returns an error for non-allowlisted values, unsafe paths, malformed
    /// existing TOML, failed atomic persistence, or an invalid merged result.
    pub fn persist_tui_setting(&self, key: &str, value: &str) -> Result<LoadedConfig, ConfigError> {
        let effective = self.load()?;
        validate_tui_setting(&effective.config, key, value)?;
        if matches!(
            key,
            "budget.session_cost_cap_micros_usd"
                | "budget.daily_cost_cap_micros_usd"
                | "budget.session_token_cap"
                | "budget.daily_token_cap"
                | "budget.token_rate_alarm_per_minute"
                | "budget.warn_at_percent"
        ) && matches!(
            effective.provenance(key),
            Some(ConfigSource::ProjectFile(_))
        ) {
            return Err(ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: "a trusted project configuration already sets this budget; edit the project's config file to change it".to_owned(),
            });
        }
        let provenance_value = match key {
            "budget.session_cost_cap_micros_usd" | "budget.daily_cost_cap_micros_usd" => {
                parse_tui_budget_cap(key, value)?
                    .map_or_else(|| "unlimited".to_owned(), |micros| micros.to_string())
            }
            "budget.session_token_cap"
            | "budget.daily_token_cap"
            | "budget.token_rate_alarm_per_minute" => parse_tui_token_limit(key, value)?
                .map_or_else(|| "unlimited".to_owned(), |tokens| tokens.to_string()),
            "budget.warn_at_percent" => parse_tui_budget_warning(key, value)?.to_string(),
            _ => value.to_owned(),
        };
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let settings_lock = acquire_tui_settings_lock(parent, key)?;
        validate_tui_config_file(&self.user_path, key)?;
        let mut document = read_tui_config_document(&self.user_path)?;
        if let Some(alias) = key.strip_prefix("models.thinking.") {
            let root = document
                .as_table_mut()
                .ok_or_else(|| ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "user configuration root is not a table".to_owned(),
                })?;
            let models = root
                .entry("models".to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                .as_table_mut()
                .ok_or_else(|| ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "models configuration is not a table".to_owned(),
                })?;
            let thinking = models
                .entry("thinking".to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                .as_table_mut()
                .ok_or_else(|| ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "model thinking configuration is not a table".to_owned(),
                })?;
            thinking.insert(alias.to_owned(), toml::Value::String(value.to_owned()));
        } else if matches!(
            key,
            "budget.session_cost_cap_micros_usd"
                | "budget.daily_cost_cap_micros_usd"
                | "budget.session_token_cap"
                | "budget.daily_token_cap"
                | "budget.token_rate_alarm_per_minute"
        ) && value.trim().eq_ignore_ascii_case("unlimited")
        {
            clear_toml_leaf(&mut document, key)?;
        } else {
            set_toml_leaf(&mut document, key, value)?;
        }
        let encoded = format!(
            "# Rottweiler user settings; last updated via TUI\n{}",
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: error.to_string(),
            })?
        );
        // Record provenance first. If the config rewrite fails, the stale entry
        // cannot claim a source because loading requires its value to match.
        // The inverse order could report failure after changing the setting.
        persist_tui_provenance(parent, &self.user_path, key, &provenance_value)?;
        persist_tui_config_atomic(parent, &self.user_path, encoded.as_bytes(), key)?;
        drop(settings_lock);
        self.load()
    }

    /// Adds one provider profile after the composition layer has resolved its
    /// canonical name and fixed adapter kind.
    /// Existing profiles are never overwritten and no endpoint, client id, or
    /// credential value can enter through this path.
    ///
    /// # Errors
    ///
    /// Returns an error for conflicting existing profiles, unsafe paths, or
    /// failed atomic persistence.
    pub fn configure_provider_profile(
        &self,
        provider: &str,
        kind: &str,
    ) -> Result<LoadedConfig, ConfigError> {
        let effective = self.load()?;
        if let Some(existing) = effective.config.providers.get(provider) {
            return if existing.kind == kind {
                Ok(effective)
            } else {
                Err(ConfigError::InvalidUserSetting {
                    key: format!("providers.{provider}"),
                    reason: "an existing provider profile uses a different adapter kind".to_owned(),
                })
            };
        }
        let key = format!("providers.{provider}.kind");
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: key.clone(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, &key)?;
        validate_tui_config_file(&self.user_path, &key)?;
        let mut document = read_tui_config_document(&self.user_path)?;
        set_toml_leaf(&mut document, &key, kind)?;
        let encoded = format!(
            "# Rottweiler user settings; last updated via TUI\n{}",
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: key.clone(),
                reason: error.to_string(),
            })?
        );
        persist_tui_provenance(parent, &self.user_path, &key, kind)?;
        persist_tui_config_atomic(parent, &self.user_path, encoded.as_bytes(), &key)?;
        self.load()
    }

    /// Persists a concrete model route in a private host preference keyed by
    /// the canonical project identity. The project tree and
    /// executable trust ledger are never modified.
    ///
    /// # Errors
    ///
    /// Returns an error when the route is invalid or the private preference
    /// file is unsafe or unavailable.
    pub fn persist_tui_project_model(&self, model: &str) -> Result<LoadedConfig, ConfigError> {
        if !valid_project_model_selection(model) {
            return Err(ConfigError::InvalidUserSetting {
                key: "project.models.default".to_owned(),
                reason: "project model must be a bounded alias or concrete provider/model route"
                    .to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "project.models.default".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "project.models.default")?;
        let path = project_model_preferences_path(&self.user_path);
        let mut preferences = read_project_model_preferences(&self.user_path)?;
        preferences.insert(project_identity(&self.project_root)?, model.to_owned());
        let bytes = serde_json::to_vec_pretty(&preferences).map_err(|error| {
            ConfigError::InvalidUserSetting {
                key: "project.models.default".to_owned(),
                reason: error.to_string(),
            }
        })?;
        persist_tui_config_atomic(parent, &path, &bytes, "project.models.default")?;
        self.load()
    }

    /// Reads the private concrete model preference for this canonical project.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity or private preference file is unsafe.
    pub fn tui_project_model(&self) -> Result<Option<String>, ConfigError> {
        Ok(read_project_model_preferences(&self.user_path)?
            .get(&project_identity(&self.project_root)?)
            .filter(|model| valid_project_model_selection(model))
            .cloned())
    }

    /// Reads the user keybinding preset managed by the simple TUI settings surface.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe, oversized, malformed, or unreadable configuration.
    pub fn tui_keybinding_preset(&self) -> Result<String, ConfigError> {
        let path = self.user_path.with_file_name("keybindings.toml");
        validate_tui_config_file(&path, "ui.keybindings.preset")?;
        let document = read_bounded_tui_config_document(
            &path,
            "ui.keybindings.preset",
            MAX_TUI_AUX_CONFIG_BYTES,
        )?;
        Ok(document
            .get("preset")
            .and_then(toml::Value::as_str)
            .filter(|preset| matches!(*preset, "standard" | "vim"))
            .unwrap_or("standard")
            .to_owned())
    }

    /// Persists only the simple user keybinding preset while preserving custom bindings.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid preset or unsafe, malformed, or unwritable configuration.
    pub fn persist_tui_keybinding_preset(&self, preset: &str) -> Result<(), ConfigError> {
        if !matches!(preset, "standard" | "vim") {
            return Err(ConfigError::InvalidUserSetting {
                key: "ui.keybindings.preset".to_owned(),
                reason: "keybinding preset must be standard or vim".to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "ui.keybindings.preset".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "ui.keybindings.preset")?;
        let path = self.user_path.with_file_name("keybindings.toml");
        validate_tui_config_file(&path, "ui.keybindings.preset")?;
        let mut document = read_bounded_tui_config_document(
            &path,
            "ui.keybindings.preset",
            MAX_TUI_AUX_CONFIG_BYTES,
        )?;
        set_toml_leaf(&mut document, "preset", preset)?;
        let bytes =
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: "ui.keybindings.preset".to_owned(),
                reason: error.to_string(),
            })?;
        persist_tui_config_atomic(parent, &path, bytes.as_bytes(), "ui.keybindings.preset")
    }

    /// Lists simple user MCP enablement flags without exposing executable details.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe, oversized, malformed, or unreadable MCP configuration.
    pub fn tui_mcp_servers(&self) -> Result<Vec<(String, bool)>, ConfigError> {
        let path = self.user_path.with_file_name("mcp.toml");
        validate_tui_config_file(&path, "mcp.servers")?;
        let document =
            read_bounded_tui_config_document(&path, "mcp.servers", MAX_TUI_AUX_CONFIG_BYTES)?;
        let mut servers = document
            .get("servers")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flat_map(|servers| servers.iter())
            .filter(|(name, value)| McpServerId::validate(name).is_ok() && value.is_table())
            .map(|(name, value)| {
                (
                    name.clone(),
                    value
                        .get("enabled")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(true),
                )
            })
            .collect::<Vec<_>>();
        servers.sort_by(|left, right| left.0.cmp(&right.0));
        servers.truncate(128);
        Ok(servers)
    }

    /// Persists one existing user MCP server's enablement flag.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown server or unsafe, malformed, or unwritable configuration.
    pub fn persist_tui_mcp_enabled(&self, server: &str, enabled: bool) -> Result<(), ConfigError> {
        if McpServerId::validate(server).is_err() {
            return Err(ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP server name is invalid".to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "mcp.servers")?;
        let path = self.user_path.with_file_name("mcp.toml");
        validate_tui_config_file(&path, "mcp.servers")?;
        let mut document =
            read_bounded_tui_config_document(&path, "mcp.servers", MAX_TUI_AUX_CONFIG_BYTES)?;
        if !document
            .get("servers")
            .and_then(toml::Value::as_table)
            .is_some_and(|servers| servers.contains_key(server))
        {
            return Err(ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}.enabled"),
                reason: "MCP server is not present in the user configuration".to_owned(),
            });
        }
        let server_table = document
            .get_mut("servers")
            .and_then(toml::Value::as_table_mut)
            .and_then(|servers| servers.get_mut(server))
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}.enabled"),
                reason: "MCP server is not a table".to_owned(),
            })?;
        server_table.insert("enabled".to_owned(), toml::Value::Boolean(enabled));
        let bytes =
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}.enabled"),
                reason: error.to_string(),
            })?;
        persist_tui_config_atomic(parent, &path, bytes.as_bytes(), "mcp.servers")
    }

    /// Adds a user-scoped remote HTTPS MCP server in disabled, deferred mode.
    /// Enabling remains a separate explicit action after fingerprint approval.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name/URL, an existing server, or an
    /// unsafe, malformed, oversized, or unwritable user MCP file.
    pub fn persist_tui_mcp_http_server(
        &self,
        server: &str,
        endpoint: &str,
    ) -> Result<(), ConfigError> {
        let key = format!("mcp.servers.{server}");
        if McpServerId::validate(server).is_err() || endpoint.len() > 2_048 {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP server name or endpoint is invalid".to_owned(),
            });
        }
        let parsed = Url::parse(endpoint).map_err(|_| ConfigError::InvalidUserSetting {
            key: key.clone(),
            reason: "MCP endpoint must be an absolute HTTPS URL".to_owned(),
        })?;
        if parsed.scheme() != "https"
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP endpoint must be HTTPS without credentials, query, or fragment"
                    .to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "mcp.servers")?;
        let path = self.user_path.with_file_name("mcp.toml");
        validate_tui_config_file(&path, "mcp.servers")?;
        let mut document =
            read_bounded_tui_config_document(&path, "mcp.servers", MAX_TUI_AUX_CONFIG_BYTES)?;
        if document
            .get("servers")
            .and_then(toml::Value::as_table)
            .is_some_and(|servers| servers.contains_key(server))
        {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP server already exists".to_owned(),
            });
        }
        let servers = document
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP configuration root is not a table".to_owned(),
            })?
            .entry("servers".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP servers value is not a table".to_owned(),
            })?;
        let mut server_table = toml::map::Map::new();
        server_table.insert(
            "endpoint".to_owned(),
            toml::Value::String(endpoint.to_owned()),
        );
        server_table.insert("enabled".to_owned(), toml::Value::Boolean(false));
        server_table.insert("defer_tools".to_owned(), toml::Value::Boolean(true));
        servers.insert(server.to_owned(), toml::Value::Table(server_table));
        let bytes =
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}"),
                reason: error.to_string(),
            })?;
        persist_tui_config_atomic(parent, &path, bytes.as_bytes(), "mcp.servers")
    }

    /// Adds a user-scoped stdio MCP server in disabled, deferred mode.
    /// Environment values are written only to the private user MCP file and
    /// are never included in validation errors.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or oversized argv/environment data, an
    /// existing server, or an unsafe, malformed, oversized, or unwritable user
    /// MCP file.
    pub fn persist_tui_mcp_stdio_server(
        &self,
        server: &str,
        executable: &Path,
        args: &[String],
        environment: &[(String, String)],
    ) -> Result<(), ConfigError> {
        let key = format!("mcp.servers.{server}");
        if McpServerId::validate(server).is_err()
            || !valid_mcp_executable_and_args(executable, args)
        {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP server name or argv is invalid".to_owned(),
            });
        }
        if !valid_mcp_environment(environment) {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP environment keys or values are invalid or oversized".to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "mcp.servers")?;
        let path = self.user_path.with_file_name("mcp.toml");
        validate_tui_config_file(&path, "mcp.servers")?;
        let mut document =
            read_bounded_tui_config_document(&path, "mcp.servers", MAX_TUI_AUX_CONFIG_BYTES)?;
        if document
            .get("servers")
            .and_then(toml::Value::as_table)
            .is_some_and(|servers| servers.contains_key(server))
        {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP server already exists".to_owned(),
            });
        }
        let servers = document
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP configuration root is not a table".to_owned(),
            })?
            .entry("servers".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP servers value is not a table".to_owned(),
            })?;
        let mut command_values = Vec::with_capacity(args.len() + 1);
        command_values.push(toml::Value::String(
            executable.to_string_lossy().into_owned(),
        ));
        command_values.extend(args.iter().cloned().map(toml::Value::String));
        let environment = environment
            .iter()
            .map(|(name, value)| (name.clone(), toml::Value::String(value.clone())))
            .collect::<toml::map::Map<_, _>>();
        let mut server_table = toml::map::Map::new();
        server_table.insert("argv".to_owned(), toml::Value::Array(command_values));
        server_table.insert("environment".to_owned(), toml::Value::Table(environment));
        server_table.insert("enabled".to_owned(), toml::Value::Boolean(false));
        server_table.insert("defer_tools".to_owned(), toml::Value::Boolean(true));
        server_table.insert("read_roots".to_owned(), toml::Value::Array(Vec::new()));
        server_table.insert("write_roots".to_owned(), toml::Value::Array(Vec::new()));
        server_table.insert("allowed_domains".to_owned(), toml::Value::Array(Vec::new()));
        servers.insert(server.to_owned(), toml::Value::Table(server_table));
        let bytes =
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}"),
                reason: error.to_string(),
            })?;
        if bytes.len() > MAX_TUI_AUX_CONFIG_BYTES {
            return Err(ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}"),
                reason: "MCP configuration exceeds its size limit".to_owned(),
            });
        }
        persist_tui_config_atomic(parent, &path, bytes.as_bytes(), "mcp.servers")
    }

    /// Removes one user-scoped MCP server and its matching capability override.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown server or unsafe, malformed, or
    /// unwritable user configuration.
    pub fn remove_tui_mcp_server(&self, server: &str) -> Result<(), ConfigError> {
        if McpServerId::validate(server).is_err() {
            return Err(ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP server name is invalid".to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "mcp.servers")?;
        let path = self.user_path.with_file_name("mcp.toml");
        validate_tui_config_file(&path, "mcp.servers")?;
        let mut document =
            read_bounded_tui_config_document(&path, "mcp.servers", MAX_TUI_AUX_CONFIG_BYTES)?;
        let removed = document
            .get_mut("servers")
            .and_then(toml::Value::as_table_mut)
            .and_then(|servers| servers.remove(server));
        if removed.is_none() {
            return Err(ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}"),
                reason: "MCP server is not present in the user configuration".to_owned(),
            });
        }
        if let Some(overrides) = document
            .get_mut("capability_overrides")
            .and_then(toml::Value::as_table_mut)
        {
            overrides.remove(server);
        }
        let bytes =
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}"),
                reason: error.to_string(),
            })?;
        persist_tui_config_atomic(parent, &path, bytes.as_bytes(), "mcp.servers")
    }
}

pub(super) fn prepare_tui_config_parent(
    parent: &Path,
    user_path: &Path,
) -> Result<(), ConfigError> {
    fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
        path: user_path.to_owned(),
        source,
    })?;
    if fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ConfigError::InvalidUserSetting {
            key: "ui".to_owned(),
            reason: "user configuration parent must not be a symlink".to_owned(),
        });
    }
    #[cfg(unix)]
    fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700)).map_err(
        |source| ConfigError::Write {
            path: parent.to_owned(),
            source,
        },
    )?;
    Ok(())
}

pub(super) struct TuiSettingsProcessLock {
    parent: PathBuf,
}

impl Drop for TuiSettingsProcessLock {
    fn drop(&mut self) {
        let (active, available) =
            TUI_SETTING_PROCESS_LOCKS.get_or_init(|| (Mutex::new(BTreeSet::new()), Condvar::new()));
        // A panic while holding the registry still must release this path for
        // already-waiting writers. Future acquisitions observe the poison and
        // fail closed instead of silently trusting possibly inconsistent state.
        let mut active = active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.remove(&self.parent);
        available.notify_all();
    }
}

pub(super) fn acquire_tui_settings_process_lock(
    parent: &Path,
    key: &str,
) -> Result<TuiSettingsProcessLock, ConfigError> {
    let (active, available) =
        TUI_SETTING_PROCESS_LOCKS.get_or_init(|| (Mutex::new(BTreeSet::new()), Condvar::new()));
    let active = active.lock().map_err(|_| ConfigError::InvalidUserSetting {
        key: key.to_owned(),
        reason: "user settings lock is unavailable".to_owned(),
    })?;
    let mut active = available
        .wait_while(active, |paths| paths.contains(parent))
        .map_err(|_| ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "user settings lock is unavailable".to_owned(),
        })?;
    active.insert(parent.to_owned());
    Ok(TuiSettingsProcessLock {
        parent: parent.to_owned(),
    })
}

#[cfg(unix)]
pub(super) struct TuiSettingsLock {
    _process: TuiSettingsProcessLock,
    _descriptor: std::os::fd::OwnedFd,
}

#[cfg(unix)]
pub(super) fn acquire_tui_settings_lock(
    parent: &Path,
    key: &str,
) -> Result<TuiSettingsLock, ConfigError> {
    // Serialize this process before entering the deliberately short, fail-closed
    // cross-process lock window. A durable rewrite can include several fsyncs and
    // legitimately exceed that window under I/O pressure; making sibling threads
    // race through it would cause spurious failures and could tempt callers to
    // retry a read-modify-write with stale state.
    let process = acquire_tui_settings_process_lock(parent, key)?;
    let path = parent.join("config.toml.lock");
    let descriptor = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(|source| ConfigError::Write {
        path: path.clone(),
        source: std::io::Error::from(source),
    })?;
    let stat = rustix::fs::fstat(&descriptor).map_err(|source| ConfigError::Write {
        path: path.clone(),
        source: std::io::Error::from(source),
    })?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_nlink != 1
        || stat.st_mode & 0o077 != 0
    {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "user settings lock is unsafe".to_owned(),
        });
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    loop {
        match rustix::fs::flock(
            &descriptor,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        ) {
            Ok(()) => break,
            Err(source) => {
                let source = std::io::Error::from(source);
                if source.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                return Err(ConfigError::Write { path, source });
            }
        }
    }
    Ok(TuiSettingsLock {
        _process: process,
        _descriptor: descriptor,
    })
}

#[cfg(not(unix))]
pub(super) fn acquire_tui_settings_lock(
    parent: &Path,
    key: &str,
) -> Result<TuiSettingsProcessLock, ConfigError> {
    acquire_tui_settings_process_lock(parent, key)
}

pub(super) fn validate_tui_config_file(path: &Path, key: &str) -> Result<(), ConfigError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    #[cfg(unix)]
    let valid = {
        use std::os::unix::fs::MetadataExt as _;
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.nlink() == 1
    };
    #[cfg(not(unix))]
    let valid = metadata.is_file() && !metadata.file_type().is_symlink();
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "user configuration must be a single-link regular file".to_owned(),
        })
    }
}

pub(super) fn read_tui_config_document(path: &Path) -> Result<toml::Value, ConfigError> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    if source.trim().is_empty() {
        Ok(toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::from_str::<toml::Table>(&source)
            .map(toml::Value::Table)
            .map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                source,
            })
    }
}

pub(super) fn read_bounded_tui_config_document(
    path: &Path,
    key: &str,
    maximum: usize,
) -> Result<toml::Value, ConfigError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    if bytes.len() > maximum {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: format!("configuration exceeds its {maximum}-byte limit"),
        });
    }
    let source = String::from_utf8(bytes).map_err(|_| ConfigError::InvalidUserSetting {
        key: key.to_owned(),
        reason: "configuration is not UTF-8".to_owned(),
    })?;
    if source.trim().is_empty() {
        Ok(toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::from_str::<toml::Table>(&source)
            .map(toml::Value::Table)
            .map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                source,
            })
    }
}

pub(super) fn persist_tui_config_atomic(
    parent: &Path,
    path: &Path,
    bytes: &[u8],
    key: &str,
) -> Result<(), ConfigError> {
    let (temporary, mut file) = allocate_tui_config_temporary(parent, key)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        fs::File::open(parent)?.sync_all()
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Write {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

pub(super) fn allocate_tui_config_temporary(
    parent: &Path,
    key: &str,
) -> Result<(PathBuf, fs::File), ConfigError> {
    for _ in 0..16 {
        let nonce = TUI_SETTING_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".config-{}-{nonce}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(ConfigError::Write {
                    path: temporary,
                    source,
                });
            }
        }
    }
    Err(ConfigError::InvalidUserSetting {
        key: key.to_owned(),
        reason: "could not allocate a private temporary file".to_owned(),
    })
}

pub(super) fn tui_provenance_path(user_path: &Path) -> PathBuf {
    user_path.with_file_name("config-tui-provenance.json")
}

pub(super) fn project_model_preferences_path(user_path: &Path) -> PathBuf {
    user_path.with_file_name("project-model-preferences.json")
}

pub(super) fn project_identity(project_root: &Path) -> Result<String, ConfigError> {
    let canonical =
        fs::canonicalize(project_root).map_err(|error| ConfigError::InvalidUserSetting {
            key: "project.models.default".to_owned(),
            reason: format!("project identity is unavailable: {error}"),
        })?;
    Ok(hash_project_identity(&canonical))
}

pub(super) fn hash_project_identity(canonical: &Path) -> String {
    let mut framed = b"rw-project-identity-v1\0".to_vec();
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt as _;
        canonical.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let rendered = canonical.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = rendered.as_bytes();
    framed.extend_from_slice(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    framed.extend_from_slice(bytes);
    blake3::hash(&framed).to_hex().to_string()
}

pub(super) fn valid_project_model_selection(model: &str) -> bool {
    if model.is_empty()
        || model.len() > 512
        || model
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }
    model.split_once('/').map_or_else(
        || {
            model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        },
        |(provider, model_id)| !provider.is_empty() && !model_id.is_empty(),
    )
}

pub(super) fn valid_mcp_executable_and_args(executable: &Path, args: &[String]) -> bool {
    let executable = executable.to_string_lossy();
    Path::new(executable.as_ref()).is_absolute()
        && !executable.is_empty()
        && executable.len() <= MAX_MCP_ARG_BYTES
        && !executable.as_bytes().contains(&0)
        && args.len() < MAX_MCP_ARGV_ENTRIES
        && args.iter().all(|argument| {
            !argument.is_empty()
                && argument.len() <= MAX_MCP_ARG_BYTES
                && !argument.as_bytes().contains(&0)
        })
}

pub(super) fn valid_mcp_environment(environment: &[(String, String)]) -> bool {
    if environment.len() > MAX_MCP_ENVIRONMENT_ENTRIES {
        return false;
    }
    let mut names = BTreeSet::new();
    environment.iter().all(|(name, value)| {
        !name.is_empty()
            && name.len() <= MAX_MCP_ENVIRONMENT_NAME_BYTES
            && name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && value.len() <= MAX_MCP_ENVIRONMENT_VALUE_BYTES
            && !value.as_bytes().contains(&0)
            && names.insert(name)
    })
}

pub(super) fn read_project_model_preferences(
    user_path: &Path,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let path = project_model_preferences_path(user_path);
    validate_tui_provenance_file(&path)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    if bytes.len() > 64 * 1024 {
        return Err(ConfigError::InvalidUserSetting {
            key: "project.models.default".to_owned(),
            reason: "project model preferences exceeded their size limit".to_owned(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidUserSetting {
        key: "project.models.default".to_owned(),
        reason: error.to_string(),
    })
}

pub(super) fn read_tui_provenance(
    user_path: &Path,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let path = tui_provenance_path(user_path);
    validate_tui_provenance_file(&path)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    if bytes.len() > 64 * 1024 {
        return Err(ConfigError::InvalidUserSetting {
            key: "provenance".to_owned(),
            reason: "TUI provenance exceeded its size limit".to_owned(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidUserSetting {
        key: "provenance".to_owned(),
        reason: error.to_string(),
    })
}

pub(super) fn validate_tui_provenance_file(path: &Path) -> Result<(), ConfigError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    #[cfg(unix)]
    let valid = {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.nlink() == 1
            && metadata.permissions().mode().trailing_zeros() >= 6
    };
    #[cfg(not(unix))]
    let valid = metadata.is_file() && !metadata.file_type().is_symlink();
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidUserSetting {
            key: "provenance".to_owned(),
            reason: "TUI provenance must be a private single-link regular file".to_owned(),
        })
    }
}

pub(super) fn persist_tui_provenance(
    parent: &Path,
    user_path: &Path,
    key: &str,
    value: &str,
) -> Result<(), ConfigError> {
    let mut provenance = read_tui_provenance(user_path)?;
    provenance.insert(key.to_owned(), value.to_owned());
    let bytes = serde_json::to_vec_pretty(&provenance).map_err(|error| {
        ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: error.to_string(),
        }
    })?;
    persist_tui_config_atomic(parent, &tui_provenance_path(user_path), &bytes, key)
}

pub(super) fn configured_setting_value(config: &Config, key: &str) -> Option<String> {
    match key {
        "ui.theme" => Some(config.ui.theme.clone()),
        "compaction.auto" => Some(config.compaction.auto.to_string()),
        "permissions.default" => Some(config.permissions.default.as_str().to_owned()),
        "budget.session_cost_cap_micros_usd" => config
            .budget
            .session_cost_cap_micros_usd
            .map(|value| value.to_string()),
        "budget.daily_cost_cap_micros_usd" => config
            .budget
            .daily_cost_cap_micros_usd
            .map(|value| value.to_string()),
        "budget.session_token_cap" => config
            .budget
            .session_token_cap
            .map(|value| value.to_string()),
        "budget.daily_token_cap" => config.budget.daily_token_cap.map(|value| value.to_string()),
        "budget.token_rate_alarm_per_minute" => config
            .budget
            .token_rate_alarm_per_minute
            .map(|value| value.to_string()),
        "budget.warn_at_percent" => Some(config.budget.warn_at_percent.to_string()),
        _ if key.starts_with("models.thinking.") => config
            .models
            .thinking
            .get(key.trim_start_matches("models.thinking."))
            .map(|level| level.as_str().to_owned()),
        _ if key.starts_with("providers.") => {
            let mut segments = key.split('.');
            match (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ) {
                (Some("providers"), Some(provider), Some("kind"), None) => config
                    .providers
                    .get(provider)
                    .map(|entry| entry.kind.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(super) fn set_toml_leaf(
    document: &mut toml::Value,
    key: &str,
    value: &str,
) -> Result<(), ConfigError> {
    let segments = key.split('.').collect::<Vec<_>>();
    let Some((leaf, parents)) = segments.split_last() else {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "setting key is empty".to_owned(),
        });
    };
    let mut cursor = document;
    for segment in parents {
        let Some(table) = cursor.as_table_mut() else {
            return Err(ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: "setting parent is not a TOML table".to_owned(),
            });
        };
        cursor = table
            .entry((*segment).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    let boolean_leaf = key == "compaction.auto"
        || (segments.first() == Some(&"servers") && segments.last() == Some(&"enabled"));
    let integer_leaf = matches!(
        key,
        "budget.session_cost_cap_micros_usd"
            | "budget.daily_cost_cap_micros_usd"
            | "budget.session_token_cap"
            | "budget.daily_token_cap"
            | "budget.token_rate_alarm_per_minute"
            | "budget.warn_at_percent"
    );
    let stored = if boolean_leaf {
        toml::Value::Boolean(value == "true")
    } else if integer_leaf {
        let value = match key {
            "budget.session_cost_cap_micros_usd" | "budget.daily_cost_cap_micros_usd" => {
                parse_tui_budget_cap(key, value)?.ok_or_else(|| {
                    ConfigError::InvalidUserSetting {
                        key: key.to_owned(),
                        reason: "unlimited budget caps must clear the TOML leaf".to_owned(),
                    }
                })?
            }
            "budget.warn_at_percent" => u64::from(parse_tui_budget_warning(key, value)?),
            "budget.session_token_cap"
            | "budget.daily_token_cap"
            | "budget.token_rate_alarm_per_minute" => parse_tui_token_limit(key, value)?
                .ok_or_else(|| ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "unlimited token limits must clear the TOML leaf".to_owned(),
                })?,
            _ => unreachable!("matched integer TUI setting"),
        };
        toml::Value::Integer(
            i64::try_from(value).map_err(|_| ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: "setting value exceeds the TOML integer range".to_owned(),
            })?,
        )
    } else {
        toml::Value::String(value.to_owned())
    };
    let Some(table) = cursor.as_table_mut() else {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "setting parent is not a TOML table".to_owned(),
        });
    };
    table.insert((*leaf).to_owned(), stored);
    Ok(())
}

pub(super) fn clear_toml_leaf(document: &mut toml::Value, key: &str) -> Result<(), ConfigError> {
    let segments = key.split('.').collect::<Vec<_>>();
    let Some((leaf, parents)) = segments.split_last() else {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "setting key is empty".to_owned(),
        });
    };
    let mut cursor = document;
    for segment in parents {
        let Some(table) = cursor.as_table_mut() else {
            return Err(ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: "setting parent is not a TOML table".to_owned(),
            });
        };
        let Some(next) = table.get_mut(*segment) else {
            return Ok(());
        };
        cursor = next;
    }
    let Some(table) = cursor.as_table_mut() else {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "setting parent is not a TOML table".to_owned(),
        });
    };
    table.remove(*leaf);
    Ok(())
}
