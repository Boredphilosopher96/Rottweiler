//! Approval records mint exact stdio bytes before the connector can start effects.
use super::{
    McpApprovalStore, McpConnectionApprovalPolicy, McpError, McpServerConfig, McpTransportConfig,
};
use crate::extension_config::{DiscoveredMcpServer, DiscoveredMcpTransport};
use async_trait::async_trait;
use rw_tools::{ApprovedProtocolCommand, ProtocolChildRequest, ProtocolSandboxPolicy};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, McpError>;
fn failure(message: &str) -> McpError {
    McpError::Policy(message.to_owned())
}

impl McpApprovalStore {
    fn approved_descriptor(&self, config: &McpServerConfig) -> Result<DiscoveredMcpServer> {
        let configs = self
            .configs
            .read()
            .map_err(|_| failure("MCP approval lock unavailable"))?;
        let discovered = configs
            .get(&config.id)
            .ok_or_else(|| failure("MCP server has no trusted configuration provenance"))?;
        let expected = self
            .expected
            .read()
            .map_err(|_| failure("MCP approval lock unavailable"))?;
        let fingerprint = expected
            .get(&config.id)
            .ok_or_else(|| failure("MCP server has no trusted configuration provenance"))?;
        let approved = self
            .approved
            .lock()
            .map_err(|_| failure("MCP approval ledger unavailable"))?;
        if approved.get(config.id.as_str()) != Some(fingerprint) {
            return Err(failure(
                "MCP server configuration requires explicit approval",
            ));
        }
        if !same_transport(discovered, config) {
            return Err(failure(
                "MCP request differs from its approved configuration",
            ));
        }
        Ok(discovered.clone())
    }

    pub(super) async fn capture_stdio(
        &self,
        config: &McpServerConfig,
        roots: &[PathBuf],
        cwd: &Path,
    ) -> Result<ApprovedProtocolCommand> {
        let discovered = self.approved_descriptor(config)?;
        let request = request(config)?;
        let roots = roots.to_vec();
        let cwd = cwd.to_path_buf();
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let identities = discovered
                .attested_files
                .iter()
                .map(crate::extension_config::ContentAttestation::artifact_identity)
                .collect::<Vec<_>>();
            let executable = identities
                .iter()
                .find(|file| file.executable == request.executable)
                .ok_or_else(|| {
                    failure("MCP command is missing its approved executable identity")
                })?;
            let files = identities
                .iter()
                .filter(|file| file.executable != request.executable)
                .cloned()
                .collect::<Vec<_>>();
            ApprovedProtocolCommand::capture(&request, executable, &files, &roots, &cwd)
                .map_err(|_| failure("approved MCP command bytes changed or could not be pinned"))
        })
        .await
        .map_err(|_| failure("MCP approved byte capture worker failed"))?
    }
}

#[async_trait]
impl McpConnectionApprovalPolicy for McpApprovalStore {
    async fn approve(&self, config: &McpServerConfig) -> Result<()> {
        let discovered = self.approved_descriptor(config)?;
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            for identity in &discovered.attested_files {
                identity.validate().map_err(|_| {
                    failure("approved MCP command content identity changed before launch")
                })?;
            }
            Ok(())
        })
        .await
        .map_err(|_| failure("MCP approval verification worker failed"))?
    }
}

fn request(config: &McpServerConfig) -> Result<ProtocolChildRequest> {
    let McpTransportConfig::Stdio {
        executable,
        args,
        working_directory,
        environment,
        sandbox,
    } = &config.transport
    else {
        return Err(failure("MCP stdio approval cannot authorize HTTP"));
    };
    Ok(ProtocolChildRequest {
        executable: executable.clone(),
        args: args.clone(),
        working_directory: working_directory.clone(),
        environment: environment.clone(),
        sandbox: ProtocolSandboxPolicy {
            read_roots: sandbox.read_roots.clone(),
            write_roots: sandbox.write_roots.clone(),
            allowed_domains: sandbox.allowed_domains.clone(),
        },
    })
}

fn same_transport(discovered: &DiscoveredMcpServer, config: &McpServerConfig) -> bool {
    if discovered.enabled != config.enabled
        || discovered.defer_tools != config.defer_tools
        || discovered.tool_capabilities != config.tool_capabilities
    {
        return false;
    }
    match (&discovered.transport, &config.transport) {
        (
            DiscoveredMcpTransport::Http {
                endpoint,
                oauth_credential,
                ..
            },
            McpTransportConfig::StreamableHttp {
                endpoint: actual,
                oauth,
            },
        ) => endpoint == actual && oauth_credential.is_some() == *oauth,
        (
            DiscoveredMcpTransport::Stdio {
                argv,
                cwd,
                inherit_env,
                environment,
                read_roots,
                write_roots,
                allowed_domains,
            },
            McpTransportConfig::Stdio {
                executable,
                args,
                working_directory,
                environment: actual,
                sandbox,
            },
        ) => {
            argv.first()
                .is_some_and(|program| Path::new(program) == executable)
                && argv.get(1..) == Some(args.as_slice())
                && cwd == working_directory
                && read_roots == &sandbox.read_roots
                && write_roots == &sandbox.write_roots
                && allowed_domains == &sandbox.allowed_domains
                && same_environment(environment, inherit_env, &discovered.credentials, actual)
        }
        _ => false,
    }
}

fn same_environment(
    literal: &[(String, String)],
    inherited: &[String],
    credentials: &[crate::extension_config::CredentialBinding],
    actual: &[(String, String)],
) -> bool {
    if actual.len() > 256 {
        return false;
    }
    let mut names = std::collections::BTreeSet::new();
    actual.iter().all(|(name, value)| {
        value.len() <= 16 * 1024
            && names.insert(name)
            && if let Some((_, expected)) = literal.iter().find(|(key, _)| key == name) {
                value == expected
            } else {
                inherited.contains(name)
                    || credentials
                        .iter()
                        .any(|binding| &binding.environment == name)
            }
    }) && literal.iter().all(|(name, _)| names.contains(name))
        && credentials
            .iter()
            .all(|binding| names.contains(&binding.environment))
}

#[cfg(test)]
mod tests {
    use super::same_environment;
    #[test]
    fn approved_environment_rejects_overrides_duplicates_and_missing_secrets() {
        let literal = vec![("MODE".into(), "approved".into())];
        let inherited = vec!["PATH".into()];
        let credentials = vec![crate::extension_config::CredentialBinding {
            environment: "TOKEN".into(),
            credential_reference: "vault:fixture".into(),
        }];
        let actual = vec![
            ("MODE".into(), "approved".into()),
            ("PATH".into(), "/tools".into()),
            ("TOKEN".into(), "resolved".into()),
        ];
        assert!(same_environment(
            &literal,
            &inherited,
            &credentials,
            &actual
        ));
        for altered in [
            vec![("MODE".into(), "changed".into()), actual[2].clone()],
            vec![
                actual[0].clone(),
                actual[2].clone(),
                ("MODE".into(), "changed".into()),
            ],
            vec![actual[0].clone()],
            vec![actual[2].clone()],
            vec![
                actual[0].clone(),
                actual[2].clone(),
                ("OTHER".into(), "value".into()),
            ],
        ] {
            assert!(!same_environment(
                &literal,
                &inherited,
                &credentials,
                &altered
            ));
        }
    }
}

#[cfg(all(test, unix))]
mod native_tests;
