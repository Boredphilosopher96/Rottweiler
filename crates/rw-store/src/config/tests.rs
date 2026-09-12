use super::editing::{acquire_tui_settings_lock, hash_project_identity};
use super::*;

use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use rw_types::config::{
    Config, PermissionDecision, ProviderAuthScheme, ProviderConfig, UpdateChannel,
};
use tempfile::tempdir;

use super::{ConfigError, ConfigLoader, ConfigSource, read_assessed_project_file};

fn make_private(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private fixture");
    }
}

mod editing;
mod layers;
