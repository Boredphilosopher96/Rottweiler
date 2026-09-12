use crate::PermissionGate;
use crate::engine::MAX_PERMISSION_APPROVALS;
use crate::engine::MAX_PERMISSION_ID_BYTES;
use crate::engine::MAX_PERMISSION_LABEL_BYTES;
use crate::engine::MAX_PERMISSION_PATTERN_BYTES;
use crate::engine::MAX_PERMISSION_RULES_PER_SCOPE;
use rw_types::ClientCommand;
use rw_types::PermissionApprovalDescriptor;
use rw_types::PermissionApprovalScope;
use rw_types::PermissionModeDescriptor;
use rw_types::PermissionRuleDescriptor;
use rw_types::PermissionStateDescriptor;
use rw_types::config::PermissionRule;

pub(super) const fn permission_mode_descriptor(
    mode: rw_types::PermissionModeDescriptor,
) -> PermissionModeDescriptor {
    mode
}

pub(super) fn permission_rule_id(scope: &str, rule: &PermissionRule) -> String {
    let mut digest = blake3::Hasher::new();
    digest.update(b"rottweiler-permission-rule-row-v1\0");
    digest.update(scope.as_bytes());
    digest.update(b"\0");
    digest.update(format!("{:?}", rule.action).as_bytes());
    digest.update(b"\0");
    digest.update(rule.pattern.as_bytes());
    format!("{scope}:{}", &digest.finalize().to_hex()[..24])
}

pub(super) fn bounded_permission_rule(
    scope: &str,
    rule: &PermissionRule,
) -> Option<PermissionRuleDescriptor> {
    (rule.pattern.len() <= MAX_PERMISSION_PATTERN_BYTES
        && !rule.pattern.chars().any(char::is_control))
    .then(|| PermissionRuleDescriptor {
        id: permission_rule_id(scope, rule),
        pattern: rule.pattern.clone(),
        action: rule.action,
    })
}

pub(in crate::engine) fn permission_state(
    permissions: &PermissionGate,
) -> PermissionStateDescriptor {
    let snapshot = permissions.snapshot();
    let mut truncated = false;
    let mut collect_rules = |scope: &str, rules: &[PermissionRule]| {
        let mut rows = Vec::new();
        for rule in rules {
            if rows.len() >= MAX_PERMISSION_RULES_PER_SCOPE {
                truncated = true;
                break;
            }
            if let Some(row) = bounded_permission_rule(scope, rule) {
                rows.push(row);
            } else {
                truncated = true;
            }
        }
        rows
    };
    let effective_rules = collect_rules("effective", &snapshot.rules);
    let session_rules = collect_rules("session", &snapshot.session_rules);
    let remembered = permissions.approval_snapshot();
    let mut approvals = Vec::new();
    for (scope, rows) in [
        (PermissionApprovalScope::Session, remembered.session),
        (PermissionApprovalScope::Project, remembered.project),
    ] {
        for approval in rows {
            if approvals.len() >= MAX_PERMISSION_APPROVALS {
                truncated = true;
                break;
            }
            if approval.id.len() > MAX_PERMISSION_ID_BYTES
                || approval.tool_name.len() > MAX_PERMISSION_LABEL_BYTES
                || approval.canonical_summary.len() > MAX_PERMISSION_LABEL_BYTES
                || approval.id.chars().any(char::is_control)
                || approval.tool_name.chars().any(char::is_control)
                || approval.canonical_summary.chars().any(char::is_control)
            {
                truncated = true;
                continue;
            }
            approvals.push(PermissionApprovalDescriptor {
                id: approval.id,
                scope,
                tool_name: approval.tool_name,
                summary: approval.canonical_summary,
            });
        }
    }
    PermissionStateDescriptor {
        default: snapshot.default,
        runtime_mode: snapshot.runtime_mode.map(permission_mode_descriptor),
        effective_rules,
        // Project configuration cannot grant permission authority. Remembered
        // project approvals are represented separately above.
        project_rules: Vec::new(),
        session_rules,
        approvals,
        truncated,
    }
}

pub(super) fn apply_permission_command(
    command: &ClientCommand,
    permissions: &PermissionGate,
) -> Result<PermissionStateDescriptor, String> {
    match command {
        ClientCommand::ListPermissions { .. } => {}
        ClientCommand::AddSessionPermissionRule {
            pattern, action, ..
        } => {
            if pattern.is_empty()
                || pattern.len() > MAX_PERMISSION_PATTERN_BYTES
                || pattern.chars().any(char::is_control)
            {
                return Err("permission rule is empty or exceeds its safety limit".to_owned());
            }
            permissions.add_session_rule(PermissionRule {
                pattern: pattern.clone(),
                action: *action,
            })?;
        }
        ClientCommand::RemoveSessionPermissionRule { rule_id, .. } => {
            if rule_id.is_empty() || rule_id.len() > MAX_PERMISSION_ID_BYTES {
                return Err("permission rule id is invalid".to_owned());
            }
            let snapshot = permissions.snapshot();
            let pattern = snapshot
                .session_rules
                .iter()
                .find(|rule| permission_rule_id("session", rule) == *rule_id)
                .map(|rule| rule.pattern.clone())
                .ok_or_else(|| "permission rule is no longer present".to_owned())?;
            if !permissions.remove_session_rule(&pattern) {
                return Err("permission rule is no longer present".to_owned());
            }
        }
        ClientCommand::RevokePermissionApproval {
            approval_id, scope, ..
        } => {
            if approval_id.is_empty() || approval_id.len() > MAX_PERMISSION_ID_BYTES {
                return Err("permission approval id is invalid".to_owned());
            }
            let approvals = permissions.approval_snapshot();
            let known = match scope {
                PermissionApprovalScope::Session => approvals.session,
                PermissionApprovalScope::Project => approvals.project,
            }
            .iter()
            .any(|approval| approval.id == *approval_id);
            if !known {
                return Err("permission approval is no longer present".to_owned());
            }
            let removed = match scope {
                PermissionApprovalScope::Session => {
                    permissions.revoke_session_approvals(Some(approval_id))
                }
                PermissionApprovalScope::Project => permissions
                    .revoke_project_approvals(Some(approval_id))
                    .map_err(|_| "project approval revocation failed".to_owned())?,
            };
            if removed != 1 {
                return Err("permission approval is no longer present".to_owned());
            }
        }
        _ => return Err("command is not a permission-management operation".to_owned()),
    }
    Ok(permission_state(permissions))
}
