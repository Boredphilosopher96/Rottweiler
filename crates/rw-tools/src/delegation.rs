//! Nested tools execute only inside an already authorized outer invocation.
mod host;
pub use host::{DelegatedTools, ToolEffectHost};

use crate::{
    CapabilityManifest, MutationScope, SubagentLifecycleMode, Tool, ToolBehavior, ToolContext,
    ToolError,
};
use rw_types::ToolCapability;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Source-owned effect boundary. Process, inference and interactive tools do
/// not acquire delegation merely by declaring filesystem or network flags.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DelegatedEffect {
    Denied,
    Filesystem,
    Http { host: String },
}

/// Immutable limits captured after outer permission approval and checkpoint
/// creation. A nested call cannot obtain another approval while holding it.
pub struct ToolEffectScope {
    approved: CapabilityManifest,
    checkpoint: CheckpointExtent,
}
enum CheckpointExtent {
    None,
    Paths(Vec<PathBuf>),
    Workspace,
}

/// An approved plugin declaration narrows the outer host scope. Domain names
/// use the same normalized host/subdomain matching as the egress boundary.
#[derive(Clone)]
pub struct ToolEffectGrant {
    capabilities: CapabilityManifest,
    domains: Arc<[String]>,
}
impl ToolEffectGrant {
    /// # Errors
    /// Rejects malformed or oversized immutable network authority.
    pub fn new(capabilities: CapabilityManifest, domains: &[String]) -> Result<Self, ToolError> {
        if domains.len() > 128 {
            return Err(denied("network authority exceeds 128 domains"));
        }
        let mut normalized = Vec::with_capacity(domains.len());
        for domain in domains {
            let value = rw_sandbox::normalize_egress_domain(domain)
                .ok_or_else(|| denied("invalid delegated network domain"))?;
            if value != *domain || normalized.contains(&value) {
                return Err(denied("network authority must be normalized and unique"));
            }
            normalized.push(value);
        }
        Ok(Self {
            capabilities,
            domains: normalized.into(),
        })
    }
    pub(crate) fn domains(&self) -> Arc<[String]> {
        self.domains.clone()
    }
    fn allows_host(&self, host: &str) -> bool {
        rw_sandbox::EgressPolicy::new(self.domains.iter()).allows_domain(host)
    }
}
impl ToolEffectScope {
    /// Captures canonical checkpoint paths using the same pinned workspace as
    /// the eventual nested tool. Path checkpoints authorize exact files.
    ///
    /// # Errors
    /// Rejects invalid paths or a checkpoint larger than the owned scope bound.
    pub fn new(
        context: &ToolContext,
        approved: CapabilityManifest,
        checkpoint: &MutationScope,
    ) -> Result<Self, ToolError> {
        let checkpoint = match checkpoint {
            MutationScope::None => CheckpointExtent::None,
            MutationScope::OpaqueWorkspace => CheckpointExtent::Workspace,
            MutationScope::Paths(paths) => {
                if paths.is_empty() || paths.len() > 128 {
                    return Err(denied("checkpoint path count is outside delegation bounds"));
                }
                CheckpointExtent::Paths(
                    paths
                        .iter()
                        .map(|path| context.resolve_writable(path))
                        .collect::<Result<Vec<_>, _>>()?,
                )
            }
        };
        Ok(Self {
            approved,
            checkpoint,
        })
    }

    /// Validates both tool-owned dynamic semantics and the approved plugin
    /// declaration. The returned context narrows HTTP redirects too.
    ///
    /// # Errors
    /// Returns an explicit denial before any nested tool code executes.
    pub fn authorize(
        &self,
        context: &ToolContext,
        grant: &ToolEffectGrant,
        tool: &dyn Tool,
        input: &Value,
    ) -> Result<ToolContext, ToolError> {
        context.cancellation.check()?;
        if tool.subagent_lifecycle_mode() != SubagentLifecycleMode::None {
            return Err(denied("subagent lifecycle cannot be delegated"));
        }
        let capabilities = tool.invocation_capabilities(input)?;
        for capability in capabilities.capabilities() {
            if !self.approved.contains(capability) || !grant.capabilities.contains(capability) {
                return Err(denied("nested capabilities exceed the approved invocation"));
            }
        }
        match tool.delegated_effect(input)? {
            DelegatedEffect::Denied => {
                return Err(denied("tool does not expose a delegated effect"));
            }
            DelegatedEffect::Filesystem => {
                self.authorize_files(context, tool, input, &capabilities)?
            }
            DelegatedEffect::Http { host } => {
                if tool.behavior() != ToolBehavior::WebFetch
                    || capabilities.capabilities() != [ToolCapability::Network]
                    || !matches!(tool.mutation_scope(input), MutationScope::None)
                    || !grant.allows_host(&host)
                {
                    return Err(denied(
                        "HTTP request exceeds the delegated network boundary",
                    ));
                }
            }
        }
        Ok(context.clone().with_effect_domains(grant.domains()))
    }

    fn authorize_files(
        &self,
        context: &ToolContext,
        tool: &dyn Tool,
        input: &Value,
        capabilities: &CapabilityManifest,
    ) -> Result<(), ToolError> {
        if !matches!(
            tool.behavior(),
            ToolBehavior::Standard | ToolBehavior::FileMutation
        ) || capabilities.capabilities().is_empty()
            || capabilities.capabilities().iter().any(|effect| {
                !matches!(
                    effect,
                    ToolCapability::ReadFilesystem | ToolCapability::WriteFilesystem
                )
            })
        {
            return Err(denied("tool is not a bounded filesystem effect"));
        }
        let paths = tool.workspace_paths(input)?;
        if paths.is_empty() || paths.len() > 128 {
            return Err(denied("filesystem effect must declare bounded input paths"));
        }
        for path in paths {
            context.resolve_writable(&path)?;
        }
        match tool.mutation_scope(input) {
            MutationScope::None if !capabilities.contains(&ToolCapability::WriteFilesystem) => {
                Ok(())
            }
            MutationScope::Paths(paths)
                if capabilities.contains(&ToolCapability::WriteFilesystem)
                    && !paths.is_empty()
                    && paths.len() <= 128 =>
            {
                for path in paths {
                    let canonical = context.resolve_writable(&path)?;
                    match &self.checkpoint {
                        CheckpointExtent::Workspace => {}
                        CheckpointExtent::Paths(covered) if covered.contains(&canonical) => {}
                        _ => {
                            return Err(denied(
                                "nested mutation exceeds outer checkpoint coverage",
                            ));
                        }
                    }
                }
                Ok(())
            }
            _ => Err(denied("nested mutation has no bounded checkpoint coverage")),
        }
    }
}
fn denied(message: &str) -> ToolError {
    ToolError::DelegationDenied(message.to_owned())
}

#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod tests;
