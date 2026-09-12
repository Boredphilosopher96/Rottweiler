//! Frame and typed request storage are admitted before either decoder allocates.
use crate::{
    ingress::{
        envelope::{Envelope, EnvelopeKind},
        frame::{READ_SCRATCH_BYTES, RawFrame, STDIO_FRAME_BYTES},
        profile,
    },
    payload_work::Allocation,
};
use rmcp::{
    ErrorData,
    model::{self as m, ClientRequest, RequestId},
};
use std::{io, mem::size_of};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};

pub(super) struct Reader<R> {
    input: BufReader<R>,
    frame: RawFrame,
}
impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(input: R) -> io::Result<Self> {
        let frame = RawFrame::new(STDIO_FRAME_BYTES).map_err(io::Error::other)?;
        Ok(Self {
            input: BufReader::with_capacity(READ_SCRATCH_BYTES, input),
            frame,
        })
    }
    /// All progress lives in `self`; losing a select branch never loses a partial line.
    pub async fn next(&mut self) -> io::Result<Option<RawFrame>> {
        loop {
            let bytes = self.input.fill_buf().await?;
            if bytes.is_empty() {
                return if self.frame.bytes.is_empty() {
                    Ok(None)
                } else {
                    self.take().map(Some)
                };
            }
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let length = newline.map_or(bytes.len(), |position| position + 1);
            self.frame
                .append(&bytes[..length])
                .map_err(io::Error::other)?;
            self.input.consume(length);
            if newline.is_some() {
                return self.take().map(Some);
            }
        }
    }
    fn take(&mut self) -> io::Result<RawFrame> {
        let next = RawFrame::new(STDIO_FRAME_BYTES).map_err(io::Error::other)?;
        Ok(std::mem::replace(&mut self.frame, next))
    }
}

pub(super) enum Body {
    Request {
        id: RequestId,
        request: Box<Result<ClientRequest, ErrorData>>,
    },
    Cancel(RequestId),
    Ignore,
}
pub(super) struct Decoded {
    pub body: Body,
    pub retained: Allocation,
}
struct DecodeWork {
    raw: RawFrame,
    retained: Allocation,
}
impl DecodeWork {
    fn run(mut self) -> io::Result<Decoded> {
        let shape = profile::inspect(&self.raw.bytes)?;
        // Direct concrete request decoding has metadata flattening and parameter
        // trials. No ClientRequest/JSON-RPC union is speculatively decoded.
        let slot = [
            size_of::<serde_json::Value>(),
            size_of::<m::ClientCapabilities>(),
            size_of::<m::InitializeRequestParams>(),
            size_of::<m::Implementation>(),
            size_of::<m::CallToolRequestParams>(),
            size_of::<m::SubscriptionFilter>(),
            size_of::<ClientRequest>(),
        ]
        .into_iter()
        .max()
        .unwrap_or(128)
        .max(128);
        self.retained
            .ensure(profile::working_bytes(&shape, slot, 12)?)?;
        let route = Envelope::read(&self.raw.bytes)?.route()?;
        let method = route.method.as_deref().unwrap_or("");
        let body = match route.kind {
            EnvelopeKind::Request => Body::Request {
                id: route
                    .id
                    .ok_or_else(|| io::Error::other("MCP request lacks ID"))?
                    .owned()?,
                request: Box::new(request(&self.raw.bytes, method).map_err(|_| {
                    ErrorData::invalid_params("invalid MCP request parameters", None)
                })),
            },
            EnvelopeKind::Notification if method == "notifications/cancelled" => {
                let value: m::CancelledNotification = serde_json::from_slice(&self.raw.bytes)?;
                value.params.request_id.map_or(Body::Ignore, Body::Cancel)
            }
            // This server sends no requests. Unsolicited responses, errors and
            // notifications carry no authority or retained connection state.
            _ => Body::Ignore,
        };
        drop(self.raw);
        Ok(Decoded {
            body,
            retained: self.retained,
        })
    }
}
pub(super) async fn decode(raw: RawFrame) -> io::Result<Decoded> {
    let work = DecodeWork {
        raw,
        retained: Allocation::new(0).map_err(io::Error::other)?,
    };
    rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
        .await
        .map_err(|_| io::Error::other("MCP request decoder worker failed"))?
}

#[allow(deprecated)] // Legacy external request names retain their existing protocol errors.
fn request(bytes: &[u8], method: &str) -> Result<ClientRequest, serde_json::Error> {
    macro_rules! parse {
        ($variant:ident) => {
            serde_json::from_slice::<m::$variant>(bytes).map(ClientRequest::$variant)
        };
    }
    match method {
        "initialize" => parse!(InitializeRequest),
        "ping" => parse!(PingRequest),
        "server/discover" => parse!(DiscoverRequest),
        "completion/complete" => parse!(CompleteRequest),
        "tools/call" => parse!(CallToolRequest),
        "tools/list" => parse!(ListToolsRequest),
        "prompts/list" => parse!(ListPromptsRequest),
        "prompts/get" => parse!(GetPromptRequest),
        "resources/list" => parse!(ListResourcesRequest),
        "resources/templates/list" => parse!(ListResourceTemplatesRequest),
        "resources/read" => parse!(ReadResourceRequest),
        "resources/subscribe" => parse!(SubscribeRequest),
        "resources/unsubscribe" => parse!(UnsubscribeRequest),
        "subscriptions/listen" => parse!(SubscriptionsListenRequest),
        "logging/setLevel" => parse!(SetLevelRequest),
        "tasks/get" => parse!(GetTaskRequest),
        "tasks/update" => parse!(UpdateTaskRequest),
        "tasks/cancel" => parse!(CancelTaskRequest),
        _ => parse!(CustomRequest),
    }
}
