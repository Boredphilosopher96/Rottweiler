//! Bounded JSON/SSE bodies, resume cursors, and connection-owned GET traffic.
use super::{
    state::{Shared, invalid},
    worker::Response,
};
use crate::client::ingress::{
    frame::RawFrame,
    sse::{SseMetadata, SseParser},
};
use crate::{McpError, McpHttpBody, McpHttpMethod, payload_work::Allocation};
use rmcp::model::ServerJsonRpcMessage;
use std::{sync::Arc, time::Duration};

const SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;
struct Cursor {
    metadata: Option<Arc<SseMetadata>>,
    retry: Duration,
}
impl Default for Cursor {
    fn default() -> Self {
        Self {
            metadata: None,
            retry: Duration::from_secs(1),
        }
    }
}
impl Cursor {
    fn id(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.id.as_deref())
            .filter(|id| !id.is_empty())
    }
    fn update(&mut self, metadata: Arc<SseMetadata>) -> Result<(), McpError> {
        if let Some(retry) = metadata.retry {
            self.retry = Duration::from_millis(retry);
        }
        if let Some(id) = &metadata.id {
            if !id.is_empty() && !super::state::valid_id(id, 512) {
                return Err(invalid());
            }
            self.metadata = Some(metadata);
        }
        Ok(())
    }
}
impl Shared {
    pub(super) async fn consume(
        self: &Arc<Self>,
        response: Response,
        expected: bool,
    ) -> Result<(), McpError> {
        let error_only = !(200..300).contains(&response.status);
        if !error_only
            && !expected
            && response
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("content-length") && value == "0")
        {
            response.discard();
            return Ok(());
        }
        let media_type = content_type(&response)?;
        if media_type == "application/json" {
            let mut response = response;
            let mut frame = RawFrame::new(super::super::frame::HTTP_BODY_BYTES)?;
            while let Some(chunk) = response.chunks.recv().await {
                let chunk = chunk?;
                frame.append(&chunk.bytes)?;
            }
            response.complete();
            self.deliver(frame, error_only).await?;
            return Ok(());
        }
        if error_only || media_type != "text/event-stream" {
            return Err(invalid());
        }
        let mut cursor = Cursor::default();
        let mut response = response;
        loop {
            if self.events(response, &mut cursor, expected).await? {
                return Ok(());
            }
            if !expected {
                return Ok(());
            }
            if cursor.id().is_none() {
                return Err(invalid());
            }
            self.delay(cursor.retry).await?;
            response = self.get(cursor.id()).await?;
            if response.status != 200 || content_type(&response)? != "text/event-stream" {
                return Err(invalid());
            }
        }
    }
    async fn events(
        self: &Arc<Self>,
        mut response: Response,
        cursor: &mut Cursor,
        stop_at_response: bool,
    ) -> Result<bool, McpError> {
        let mut parser = SseParser::new(SSE_EVENT_BYTES)?;
        loop {
            let chunk = tokio::select! {
                chunk = response.chunks.recv() => chunk,
                () = self.closing.cancelled() => { response.discard(); return Ok(false); }
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk?;
            let mut remaining = chunk.bytes.as_slice();
            while !remaining.is_empty() {
                let (read, event) = parser.push(remaining)?;
                remaining = &remaining[read..];
                if let Some(event) = event {
                    cursor.update(Arc::clone(&event.metadata))?;
                    if let Some(frame) = event.data {
                        // SSE priming events carry resume/retry metadata without
                        // a JSON-RPC message. Keep that cursor and continue.
                        if std::str::from_utf8(&frame.bytes)
                            .map_err(|_| invalid())?
                            .trim()
                            .is_empty()
                        {
                            continue;
                        }
                        let terminal = match self.deliver(frame, false).await {
                            Ok(terminal) => terminal,
                            Err(_) if self.closing.is_cancelled() => {
                                response.discard();
                                return Ok(false);
                            }
                            Err(error) => return Err(error),
                        };
                        if terminal && stop_at_response {
                            response.discard();
                            return Ok(true);
                        }
                    }
                }
            }
        }
        parser.finish();
        response.complete();
        Ok(false)
    }
    async fn deliver(
        self: &Arc<Self>,
        frame: RawFrame,
        error_only: bool,
    ) -> Result<bool, McpError> {
        let mut packet = Arc::clone(&self.ingress).decode(frame).await?;
        if error_only && !matches!(packet.message, ServerJsonRpcMessage::Error(_)) {
            return Err(invalid());
        }
        let shared = Arc::clone(self);
        let packet = self
            .ingress
            .jobs
            .run(
                rw_resources::ResourceClass::Cpu,
                self.stopped.clone(),
                move |_| {
                    shared.observe(&mut packet)?;
                    Ok::<_, McpError>(packet)
                },
            )
            .await??;
        let terminal = matches!(
            packet.message,
            ServerJsonRpcMessage::Response(_) | ServerJsonRpcMessage::Error(_)
        );
        tokio::select! { result = self.sender.send(packet) => result.map_err(|_| invalid())?, () = self.stopped.cancelled() => return Err(invalid()) }
        Ok(terminal)
    }
    async fn get(&self, id: Option<&str>) -> Result<Response, McpError> {
        let retained = Arc::new(Allocation::new(64 * 1024)?);
        let headers = self.headers(Vec::new(), id, false)?;
        self.request(
            McpHttpMethod::Get,
            headers,
            McpHttpBody::new(Vec::new(), retained),
        )
        .await
    }
    pub(super) async fn get_stream(self: &Arc<Self>) -> Result<(), McpError> {
        let mut cursor = Cursor::default();
        let mut attempts = 0_u32;
        loop {
            if self.closing.is_cancelled() {
                return Ok(());
            }
            let response = match self.get(cursor.id()).await {
                Ok(response) => response,
                Err(McpError::Transport) => {
                    self.backoff(&mut attempts).await?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if response.status == 405 {
                response.discard();
                return Ok(());
            }
            if response.status == 200 && content_type(&response)? == "text/event-stream" {
                attempts = 0;
                match self.events(response, &mut cursor, false).await {
                    Ok(_) => {}
                    Err(McpError::Transport) => {
                        self.backoff(&mut attempts).await?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
                self.delay(cursor.retry).await?;
            } else {
                response.discard();
                self.backoff(&mut attempts).await?;
            }
        }
    }
    async fn backoff(&self, attempts: &mut u32) -> Result<(), McpError> {
        let millis = 1_000_u64.checked_shl(*attempts).ok_or_else(invalid)?;
        *attempts = attempts.checked_add(1).ok_or_else(invalid)?;
        self.delay(Duration::from_millis(millis)).await
    }
    async fn delay(&self, duration: Duration) -> Result<(), McpError> {
        let deadline = tokio::time::Instant::now()
            .checked_add(duration)
            .ok_or_else(invalid)?;
        tokio::select! { () = tokio::time::sleep_until(deadline) => Ok(()), () = self.stopped.cancelled() => Err(invalid()), () = self.closing.cancelled() => Err(invalid()) }
    }
}
fn content_type(response: &Response) -> Result<&str, McpError> {
    let mut values = response
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"));
    let value = values.next().map(|(_, value)| value).ok_or_else(invalid)?;
    if values.next().is_some() {
        return Err(invalid());
    }
    Ok(value.split(';').next().unwrap_or("").trim())
}
