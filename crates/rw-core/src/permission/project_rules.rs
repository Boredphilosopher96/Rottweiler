//! Durable, user-reviewed permission rules for one workspace.
//!
//! Rules live in the user's private storage beside the project approval
//! ledger, never in repository files, so a cloned project cannot plant
//! authority. They use the same `tool(glob)` syntax and precedence as session
//! rules: explicit deny rules still win, and pattern rules never bypass mode
//! overlays, sandbox validation, or network authority.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use rw_types::config::PermissionRule;

use super::project_store::{
    CrossProcessApprovalLock, load_private_ledger, normalize_approval_path, persist_private_ledger,
    sibling_path,
};
use super::{lock_mutex, rules::validate_rule};

/// Upper bound on durable rules per workspace, so the ledger stays small and
/// every rule remains reviewable in `/permissions`.
pub(super) const MAX_PROJECT_RULES: usize = 256;

pub(super) struct ProjectRuleStore {
    path: PathBuf,
    transaction: Mutex<()>,
}

impl ProjectRuleStore {
    /// Reads the ledger. Rules written by another process apply immediately;
    /// an unreadable or unsafe ledger fails closed by contributing no rules.
    pub(super) fn refresh(&self) -> Vec<PermissionRule> {
        let _transaction = lock_mutex(&self.transaction);
        let Ok(_file_lock) = CrossProcessApprovalLock::acquire(&self.path) else {
            return Vec::new();
        };
        load_rules(&self.path).unwrap_or_default()
    }

    pub(super) fn add(&self, rule: PermissionRule) -> Result<(), String> {
        validate_rule(&rule.pattern)?;
        self.update(|rules| {
            rules.retain(|existing| existing.pattern != rule.pattern);
            if rules.len() >= MAX_PROJECT_RULES {
                return Err(format!(
                    "this project already has {MAX_PROJECT_RULES} saved rules; remove one in /permissions first"
                ));
            }
            rules.push(rule);
            Ok(())
        })
    }

    pub(super) fn remove(&self, pattern: &str) -> Result<bool, String> {
        let mut removed = false;
        self.update(|rules| {
            let before = rules.len();
            rules.retain(|rule| rule.pattern != pattern);
            removed = rules.len() != before;
            Ok(())
        })?;
        Ok(removed)
    }

    fn update(
        &self,
        change: impl FnOnce(&mut Vec<PermissionRule>) -> Result<(), String>,
    ) -> Result<(), String> {
        let _transaction = lock_mutex(&self.transaction);
        let _file_lock = CrossProcessApprovalLock::acquire(&self.path)
            .map_err(|error| format!("project rules are unavailable: {error}"))?;
        let mut rules = load_rules(&self.path)
            .map_err(|error| format!("project rules are unreadable: {error}"))?;
        let original = rules.clone();
        change(&mut rules)?;
        if rules != original {
            persist_private_ledger(&self.path, &rules)
                .map_err(|error| format!("project rules could not be saved: {error}"))?;
        }
        Ok(())
    }
}

/// Loads a ledger and drops any entry that is not a valid rule, so a damaged
/// entry can never widen authority beyond what validation accepts.
fn load_rules(path: &Path) -> Result<Vec<PermissionRule>, std::io::Error> {
    let mut rules: Vec<PermissionRule> = load_private_ledger(path, "project rule ledger")?;
    rules.retain(|rule| validate_rule(&rule.pattern).is_ok());
    rules.truncate(MAX_PROJECT_RULES);
    Ok(rules)
}

/// The rule ledger path for a project approval ledger path.
pub(super) fn project_rules_path(approval_path: &Path) -> Option<PathBuf> {
    sibling_path(approval_path, "rules").ok()
}

pub(super) fn shared_project_rule_store(path: &Path) -> Arc<ProjectRuleStore> {
    static STORES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<ProjectRuleStore>>>> = OnceLock::new();
    let normalized = normalize_approval_path(path);
    let registry = STORES.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut registry = lock_mutex(registry);
    Arc::clone(registry.entry(normalized.clone()).or_insert_with(|| {
        Arc::new(ProjectRuleStore {
            path: normalized,
            transaction: Mutex::new(()),
        })
    }))
}
