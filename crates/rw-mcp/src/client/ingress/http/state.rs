use super::super::{Ingress, http_headers::ToolHeaderAnnotations, message::InboundPacket};
use crate::{
    McpError, McpHttpBody, McpHttpClient, McpHttpMethod, SecretToken, payload_work::Allocation,
};
use rmcp::model::ProtocolVersion;
use rw_tools::CancellationToken;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, mpsc};

pub(super) struct Schema {
    pub(super) annotations: ToolHeaderAnnotations,
    pub(super) _name: Allocation,
}
pub(super) struct Session {
    pub(super) id: Option<String>,
    pub(super) version: ProtocolVersion,
    pub(super) initialized: bool,
    pub(super) deleted: bool,
    pub(super) schemas: BTreeMap<String, Schema>,
}
pub(super) struct Shared {
    pub(super) token: Option<SecretToken>,
    pub(super) runtime: super::worker::RuntimeHost,
    pub(super) ingress: Arc<Ingress>,
    pub(super) sender: mpsc::Sender<InboundPacket>,
    pub(super) session: Mutex<Session>,
    pub(super) stopped: CancellationToken,
    pub(super) closing: CancellationToken,
    pub(super) changed: Notify,
    _metadata: Allocation,
}
impl Shared {
    pub(super) fn new(
        endpoint: String,
        token: Option<SecretToken>,
        client: Arc<dyn McpHttpClient>,
        ingress: Arc<Ingress>,
        sender: mpsc::Sender<InboundPacket>,
    ) -> Result<Arc<Self>, McpError> {
        if endpoint.len() > 8192
            || token
                .as_ref()
                .is_some_and(|token| token.expose().is_empty() || token.expose().len() > 8192)
        {
            return Err(invalid());
        }
        let metadata = Allocation::new(256 * 1024)?;
        let stopped = CancellationToken::default();
        let runtime =
            super::worker::RuntimeHost::new(client, endpoint, stopped.clone(), &ingress.jobs)?;
        Ok(Arc::new(Self {
            token,
            runtime,
            ingress,
            sender,
            session: Mutex::new(Session {
                id: None,
                version: ProtocolVersion::default(),
                initialized: false,
                deleted: false,
                schemas: BTreeMap::new(),
            }),
            stopped,
            closing: CancellationToken::default(),
            changed: Notify::new(),
            _metadata: metadata,
        }))
    }
    pub(super) fn should_start_get(&self) -> Result<bool, McpError> {
        let session = self.session.lock().map_err(|_| invalid())?;
        Ok(session.initialized && session.id.is_some())
    }
    pub(super) fn headers(
        &self,
        mut standard: Vec<(String, String)>,
        last_event: Option<&str>,
        json: bool,
    ) -> Result<Vec<(String, String)>, McpError> {
        standard.push((
            "accept".into(),
            "text/event-stream, application/json".into(),
        ));
        if json {
            standard.push(("content-type".into(), "application/json".into()));
        }
        if let Some(token) = &self.token {
            standard.push(("authorization".into(), format!("Bearer {}", token.expose())));
        }
        let session = self.session.lock().map_err(|_| invalid())?;
        if let Some(id) = &session.id {
            standard.push(("mcp-session-id".into(), id.clone()));
        }
        if !standard
            .iter()
            .any(|(name, _)| name == "mcp-protocol-version")
        {
            standard.push((
                "mcp-protocol-version".into(),
                session.version.as_str().to_owned(),
            ));
        }
        if let Some(id) = last_event {
            if !valid_id(id, 512) {
                return Err(invalid());
            }
            standard.push(("last-event-id".into(), id.to_owned()));
        }
        if standard.len() > 32
            || standard
                .iter()
                .try_fold(0usize, |sum, (name, value)| {
                    sum.checked_add(name.len())?.checked_add(value.len())
                })
                .is_none_or(|sum| sum > 32 * 1024)
            || standard.iter().any(|(_, value)| value.len() > 8192)
        {
            return Err(invalid());
        }
        Ok(standard)
    }
    pub(super) async fn request(
        &self,
        method: McpHttpMethod,
        headers: Vec<(String, String)>,
        body: McpHttpBody,
    ) -> Result<super::worker::Response, McpError> {
        crate::validate_mcp_http_headers(&headers)?;
        let retained = body.retention();
        self.runtime.request(method, headers, body, retained).await
    }
    pub(super) async fn delete_session(&self) -> Result<(), McpError> {
        {
            let mut session = self.session.lock().map_err(|_| invalid())?;
            if session.deleted || session.id.is_none() {
                return Ok(());
            }
            session.deleted = true;
        }
        // Session cleanup is requested before runtime cancellation by close().
        let retained = Arc::new(Allocation::new(64 * 1024)?);
        let headers = self.headers(Vec::new(), None, false)?;
        let body = McpHttpBody::new(Vec::new(), retained);
        let response = self.request(McpHttpMethod::Delete, headers, body).await?;
        let accepted = response.status == 405 || (200..300).contains(&response.status);
        response.discard();
        if accepted { Ok(()) } else { Err(invalid()) }
    }
}
pub(super) fn valid_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}
pub(super) fn invalid() -> McpError {
    McpError::Protocol("MCP HTTP session or framing contract failed".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn session_and_resume_identifiers_reject_controls_and_oversized_values() {
        assert!(!super::valid_id("bad id", 256));
        assert!(!super::valid_id("line\r\ninjection", 256));
        assert!(!super::valid_id(&"x".repeat(513), 512));
        assert!(super::valid_id(&"x".repeat(512), 512));
    }
}
