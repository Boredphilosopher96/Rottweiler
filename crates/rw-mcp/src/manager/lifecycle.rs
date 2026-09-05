//! Connect, disable, and shutdown own their actual futures through completion.
use super::{
    McpManager, ServerEntry, catalog_fingerprint, load_catalog, operations, sanitize_catalog,
    status_message, transition::Transition,
};
use crate::{McpError, McpServerConfig, ServerState};
use futures_util::future::join_all;
use rw_types::McpServerId;
use std::sync::Arc;

impl McpManager {
    pub async fn connect_all(&self) -> Vec<(McpServerId, Result<(), McpError>)> {
        let servers = self
            .inner
            .servers
            .read()
            .await
            .values()
            .filter(|entry| entry.config.enabled && matches!(entry.state, ServerState::Connecting))
            .map(|entry| entry.config.id.clone())
            .collect::<Vec<_>>();
        join_all(servers.into_iter().map(|id| async move {
            let result = self.set_enabled(&id, true).await;
            (id, result)
        }))
        .await
    }

    pub async fn set_enabled(&self, server: &McpServerId, enabled: bool) -> Result<(), McpError> {
        let transition = {
            let mut servers = self.inner.servers.write().await;
            self.inner.operations.ensure_open()?;
            let entry = servers
                .get_mut(server)
                .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
            if enabled {
                if matches!(
                    entry.state,
                    ServerState::Ready | ServerState::ApprovalRequired
                ) && entry.catalog_valid()
                {
                    return Ok(());
                }
                if matches!(entry.state, ServerState::Stopping) {
                    return Err(McpError::NotConnected(server.clone()));
                }
                if let Some(active) = &entry.transition
                    && active.result().is_none()
                {
                    Arc::clone(active)
                } else {
                    self.begin_connection(entry)?
                }
            } else {
                self.begin_retirement(entry)?
            }
        };
        transition
            .wait(if enabled {
                self.inner.limits.request_timeout
            } else {
                self.inner.limits.shutdown_timeout
            })
            .await
    }

    pub async fn reconnect_if_failed(&self, server: &McpServerId) -> Result<bool, McpError> {
        let transition = {
            let mut servers = self.inner.servers.write().await;
            self.inner.operations.ensure_open()?;
            let entry = servers
                .get_mut(server)
                .ok_or_else(|| McpError::UnknownServer(server.clone()))?;
            if !entry.config.enabled
                || (!matches!(entry.state, ServerState::Failed { .. }) && entry.catalog_valid())
            {
                return Ok(false);
            }
            if entry
                .transition
                .as_ref()
                .is_some_and(|active| active.result().is_none())
            {
                return Ok(false);
            }
            self.begin_connection(entry)?
        };
        transition.wait(self.inner.limits.request_timeout).await?;
        Ok(true)
    }

    fn begin_connection(&self, entry: &mut ServerEntry) -> Result<Arc<Transition>, McpError> {
        let id = entry.config.id.clone();
        self.inner.operations.ensure_idle(&id)?;
        if let Some(previous) = &entry.transition
            && matches!(
                previous.result(),
                Some(Err(McpError::EffectsUnsettled { .. }))
            )
        {
            return Err(operations::unsettled(&id));
        }
        entry.generation = entry
            .generation
            .checked_add(1)
            .ok_or_else(|| operations::unsettled(&id))?;
        entry.config.enabled = true;
        entry.state = ServerState::Connecting;
        let previous_client = entry.client.take();
        let config = entry.config.clone();
        let generation = entry.generation;
        let manager = self.clone();
        let transition = Transition::start(id, move |transition| async move {
            let result = async {
                if let Some(previous) = previous_client {
                    previous
                        .close(manager.inner.limits.shutdown_timeout)
                        .await
                        .map_err(|_| operations::unsettled(&config.id))?;
                }
                manager
                    .connect_generation(config.clone(), generation, &transition)
                    .await
            }
            .await;
            if let Err(error) = &result {
                let mut servers = manager.inner.servers.write().await;
                if let Some(entry) = servers.get_mut(&config.id)
                    && entry.generation == generation
                {
                    entry.state = ServerState::Failed {
                        message: status_message(error),
                    };
                }
            }
            result
        });
        entry.transition = Some(Arc::clone(&transition));
        Ok(transition)
    }

    async fn connect_generation(
        &self,
        config: McpServerConfig,
        generation: u64,
        transition: &Transition,
    ) -> Result<(), McpError> {
        let client = self.inner.connector.connect(&config).await?;
        if transition.cancelled() {
            client
                .close(self.inner.limits.shutdown_timeout)
                .await
                .map_err(|_| operations::unsettled(&config.id))?;
            return Err(McpError::Disabled(config.id));
        }
        let catalog = load_catalog(&*client)
            .await
            .and_then(|(tools, resources, prompts)| {
                Ok((
                    sanitize_catalog(tools)?,
                    sanitize_catalog(resources)?,
                    sanitize_catalog(prompts)?,
                ))
            });
        let (tools, resources, prompts) = match catalog {
            Ok(catalog) => catalog,
            Err(error) => {
                client
                    .close(self.inner.limits.shutdown_timeout)
                    .await
                    .map_err(|_| operations::unsettled(&config.id))?;
                return Err(error);
            }
        };
        let accepted = {
            let mut servers = self.inner.servers.write().await;
            if let Some(entry) = servers.get_mut(&config.id)
                && entry.generation == generation
                && entry.config.enabled
                && !transition.cancelled()
                && client.catalog_valid()
            {
                let fingerprint = catalog_fingerprint(&tools);
                if entry.catalog_fingerprint.is_some()
                    && entry.catalog_fingerprint != Some(fingerprint)
                {
                    entry.pending_catalog = Some(tools);
                } else {
                    entry.tools = tools;
                    entry.catalog_fingerprint = Some(fingerprint);
                }
                entry.resources = resources;
                entry.prompts = prompts;
                entry.client = Some(Arc::clone(&client));
                entry.state = if entry.pending_catalog.is_some() {
                    ServerState::ApprovalRequired
                } else {
                    ServerState::Ready
                };
                true
            } else {
                false
            }
        };
        if accepted {
            return Ok(());
        }
        client
            .close(self.inner.limits.shutdown_timeout)
            .await
            .map_err(|_| operations::unsettled(&config.id))?;
        Err(McpError::Disabled(config.id))
    }

    fn begin_retirement(&self, entry: &mut ServerEntry) -> Result<Arc<Transition>, McpError> {
        if matches!(entry.state, ServerState::Stopping)
            && let Some(active) = &entry.transition
        {
            return Ok(Arc::clone(active));
        }
        let id = entry.config.id.clone();
        entry.generation = entry
            .generation
            .checked_add(1)
            .ok_or_else(|| operations::unsettled(&id))?;
        let generation = entry.generation;
        self.inner.operations.cancel_server(&id);
        entry.config.enabled = false;
        entry.state = ServerState::Stopping;
        let previous = entry.transition.take();
        if let Some(previous) = &previous {
            previous.cancel();
        }
        let client = entry.client.take();
        let manager = self.clone();
        let transition = Transition::start(id.clone(), move |_| async move {
            let connection = async {
                if let Some(previous) = previous
                    && matches!(
                        previous.completed().await,
                        Err(McpError::EffectsUnsettled { .. })
                    )
                {
                    return Err(operations::unsettled(&id));
                }
                Ok(())
            };
            let closure = async {
                if let Some(client) = client {
                    client
                        .close(manager.inner.limits.shutdown_timeout)
                        .await
                        .map_err(|_| operations::unsettled(&id))
                } else {
                    Ok(())
                }
            };
            let (connection, closure, invocations) = tokio::join!(
                connection,
                closure,
                manager.inner.operations.drain_server(&id),
            );
            let result = connection.and(closure).and(invocations);
            let mut servers = manager.inner.servers.write().await;
            if let Some(entry) = servers.get_mut(&id)
                && entry.generation == generation
            {
                entry.state = result.as_ref().map_or_else(
                    |error| ServerState::Failed {
                        message: status_message(error),
                    },
                    |()| ServerState::Disabled,
                );
            }
            result
        });
        entry.transition = Some(Arc::clone(&transition));
        Ok(transition)
    }

    pub async fn shutdown(&self) -> Vec<(McpServerId, Result<(), McpError>)> {
        let transition = {
            let mut shutdown = self
                .inner
                .shutdown
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(transition) = &*shutdown {
                Arc::clone(transition)
            } else {
                self.inner.operations.stop();
                let manager = self.clone();
                let transition =
                    Transition::start(McpServerId::from_static("shutdown"), move |_| async move {
                        let transitions = {
                            let mut servers = manager.inner.servers.write().await;
                            servers
                                .values_mut()
                                .map(|entry| manager.begin_retirement(entry))
                                .collect::<Result<Vec<_>, _>>()?
                        };
                        let results =
                            join_all(transitions.iter().map(|transition| transition.completed()))
                                .await;
                        let settled = manager
                            .inner
                            .operations
                            .settle(manager.inner.limits.shutdown_timeout)
                            .await;
                        for result in results {
                            result?;
                        }
                        settled
                    });
                *shutdown = Some(Arc::clone(&transition));
                transition
            }
        };
        let result = transition.wait(self.inner.limits.shutdown_timeout).await;
        let servers = self.inner.servers.read().await;
        if servers.is_empty() && result.is_err() {
            return vec![(McpServerId::from_static("shutdown"), result)];
        }
        servers
            .keys()
            .map(|id| (id.clone(), result.clone()))
            .collect()
    }
}
