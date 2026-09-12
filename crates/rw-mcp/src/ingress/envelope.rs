use super::{header, invalid, json_error};
use rmcp::model::RequestId;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnvelopeKind {
    Response,
    Error,
    Request,
    Notification,
}
#[derive(Debug)]
pub(crate) struct EnvelopeRoute<'a> {
    pub kind: EnvelopeKind,
    pub id: Option<BorrowedId<'a>>,
    pub method: Option<String>,
}

/// Wire identity retained without allocating an untrusted ID string.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BorrowedId<'a>(&'a RawValue);
impl BorrowedId<'_> {
    /// Preserves rmcp's exact ID equality plus numeric-string response matching.
    pub fn matches_request_id(&self, request: &RequestId) -> io::Result<bool> {
        header::matches_id(self.0.get(), request)
    }
    /// Numeric fast path, including rmcp's numeric-string response equivalence.
    pub fn numeric_value(&self) -> io::Result<Option<i64>> {
        header::numeric_id(self.0.get())
    }
    pub(crate) fn encoded_bytes(self) -> usize {
        self.0.get().len()
    }
    pub(crate) fn owned(self) -> io::Result<RequestId> {
        header::id(self.0.get())
    }
}

// Raw fields preserve presence, including explicit null. Derive rejects duplicate
// known keys (including escaped spellings) before choosing a routing identity.
#[derive(Deserialize)]
pub(crate) struct Envelope<'a> {
    #[serde(borrow)]
    jsonrpc: &'a RawValue,
    #[serde(default, borrow, deserialize_with = "present")]
    id: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "present")]
    method: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "present")]
    pub result: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "present")]
    pub error: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "present")]
    params: Option<&'a RawValue>,
    #[serde(skip)]
    pub input: &'a [u8],
}
fn present<'de, D: Deserializer<'de>>(decoder: D) -> Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(decoder).map(Some)
}
impl<'a> Envelope<'a> {
    pub fn read(input: &'a [u8]) -> io::Result<Self> {
        let mut value: Self = serde_json::from_slice(input).map_err(json_error)?;
        value.input = input;
        header::version(value.jsonrpc.get())?;
        Ok(value)
    }
    pub fn route(&self) -> io::Result<EnvelopeRoute<'a>> {
        let id = self
            .id
            .filter(|id| !(self.error.is_some() && id.get().trim() == "null"))
            .map(|id| {
                header::numeric_id(id.get())?;
                Ok::<_, io::Error>(BorrowedId(id))
            })
            .transpose()?;
        if let Some(method) = self.method {
            if self.result.is_some() || self.error.is_some() {
                return Err(invalid("MCP envelope mixes method and response fields"));
            }
            let method = header::method(method.get())?;
            return Ok(EnvelopeRoute {
                kind: if id.is_some() {
                    EnvelopeKind::Request
                } else {
                    EnvelopeKind::Notification
                },
                id,
                method: Some(method),
            });
        }
        if self.params.is_some() || self.result.is_some() == self.error.is_some() {
            return Err(invalid("MCP envelope requires exactly one response body"));
        }
        if self.result.is_some() && id.is_none() {
            return Err(invalid("MCP result requires a request ID"));
        }
        Ok(EnvelopeRoute {
            kind: if self.result.is_some() {
                EnvelopeKind::Response
            } else {
                EnvelopeKind::Error
            },
            id,
            method: None,
        })
    }
}
