//! Paginated catalogs retain every decoded page through bounded conversion.
use super::{RmcpClient, protocol, protocol_failure};
use crate::{McpError, McpResponse, McpResponseLimits, McpResponseSlot};
use rmcp::model::{
    ClientRequest, ListPromptsRequest, ListResourcesRequest, ListToolsRequest,
    PaginatedRequestParams, ServerResult,
};
use serde_json::Value;

#[derive(Clone, Copy)]
pub(super) enum Catalog {
    Tools,
    Resources,
    Prompts,
}
impl Catalog {
    fn request(self, cursor: Option<String>) -> ClientRequest {
        let params = Some(PaginatedRequestParams::default().with_cursor(cursor));
        match self {
            Self::Tools => ListToolsRequest {
                method: rmcp::model::ListToolsRequestMethod,
                params,
                extensions: rmcp::model::Extensions::default(),
            }
            .into(),
            Self::Resources => ListResourcesRequest {
                method: rmcp::model::ListResourcesRequestMethod,
                params,
                extensions: rmcp::model::Extensions::default(),
            }
            .into(),
            Self::Prompts => ListPromptsRequest {
                method: rmcp::model::ListPromptsRequestMethod,
                params,
                extensions: rmcp::model::Extensions::default(),
            }
            .into(),
        }
    }
}
// The cursor is decoded under the response allowance and must retire first
// when an abandoned CPU result is destroyed before the awaiting task resumes.
struct Page {
    cursor: Option<String>,
    response: McpResponse<Vec<Value>>,
}
struct PageWork {
    response: McpResponse<ServerResult>,
    catalog: Catalog,
}
impl PageWork {
    fn run(self) -> Result<Page, McpError> {
        let (values, cursor) = match (self.catalog, self.response.value) {
            (Catalog::Tools, ServerResult::ListToolsResult(page)) => {
                (convert(page.tools)?, page.next_cursor)
            }
            (Catalog::Resources, ServerResult::ListResourcesResult(page)) => {
                (convert(page.resources)?, page.next_cursor)
            }
            (Catalog::Prompts, ServerResult::ListPromptsResult(page)) => {
                (convert(page.prompts)?, page.next_cursor)
            }
            _ => return Err(protocol_failure()),
        };
        Ok(Page {
            cursor,
            response: McpResponse::wire(values, self.response.retained),
        })
    }
}
fn convert<T: serde::Serialize>(values: Vec<T>) -> Result<Vec<Value>, McpError> {
    if values.len() > super::MAX_PAGINATED_ENTRIES {
        return Err(protocol_failure());
    }
    values
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(protocol)
}

impl RmcpClient {
    pub(super) async fn catalog(
        &self,
        catalog: Catalog,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<Value>>, McpError> {
        let advertised = self.peer()?.peer_info().is_some_and(|info| match catalog {
            Catalog::Tools => info.capabilities.tools.is_some(),
            Catalog::Resources => info.capabilities.resources.is_some(),
            Catalog::Prompts => info.capabilities.prompts.is_some(),
        });
        if !advertised {
            return slot.adopt(Vec::new()).await;
        }
        let mut retained = vec![slot.into_retention()];
        let mut values = Vec::with_capacity(super::MAX_PAGINATED_ENTRIES);
        let mut cursor = None;
        for _ in 0..super::MAX_PAGINATED_ENTRIES {
            let response = self
                .request(
                    catalog.request(cursor.take()),
                    McpResponseSlot::new(McpResponseLimits::WIRE)?,
                )
                .await?;
            let work = PageWork { response, catalog };
            let Page {
                mut response,
                cursor: next,
            } = rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
                .await
                .map_err(protocol)??;
            if values.len().saturating_add(response.value.len()) > super::MAX_PAGINATED_ENTRIES {
                return Err(protocol_failure());
            }
            values.append(&mut response.value);
            retained.append(&mut response.retained);
            cursor = next;
            if cursor.is_none() {
                return Ok(McpResponse::wire(values, retained));
            }
        }
        Err(protocol_failure())
    }
}
