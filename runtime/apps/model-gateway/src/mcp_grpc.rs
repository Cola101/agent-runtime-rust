//! gRPC surface for federated MCP calls (ADR-0040).
//!
//! Thin on purpose. The rules live in [`crate::mcp`] -- endpoint restriction,
//! catalog freezing, response bounds, credential opening -- and this layer only
//! translates. A rule implemented here instead would be a rule the library tests
//! could not reach.

use crate::mcp::{McpFederationClient, McpFederationError, McpServerRef};
use agent_model_gateway_protocol::v1::mcp_federation_server::McpFederation;
use agent_model_gateway_protocol::v1::{
    McpCallToolRequest, McpCallToolResponse, McpListToolsRequest, McpListToolsResponse, McpTool,
};
use tonic::{Request, Response, Status};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;

pub struct McpFederationGrpcService {
    client: McpFederationClient,
}

impl McpFederationGrpcService {
    pub fn new(client: McpFederationClient) -> Self {
        Self { client }
    }
}

#[tonic::async_trait]
impl McpFederation for McpFederationGrpcService {
    async fn list_tools(
        &self,
        request: Request<McpListToolsRequest>,
    ) -> Result<Response<McpListToolsResponse>, Status> {
        let request = request.into_inner();
        let tenant_id = parse_uuid(&request.tenant_id, "tenant_id")?;
        let server = server_ref(request.server)?;
        let catalog = self
            .client
            .list_tools(tenant_id, &server)
            .await
            .map_err(to_status)?;
        Ok(Response::new(McpListToolsResponse {
            schema_version: SCHEMA_VERSION,
            catalog_digest: catalog.digest,
            tools: catalog
                .tools
                .into_iter()
                .map(|tool| McpTool {
                    qualified_name: tool.qualified_name,
                    description: tool.description,
                    input_schema_json: tool.input_schema_json.into_bytes().into(),
                })
                .collect(),
        }))
    }

    async fn call_tool(
        &self,
        request: Request<McpCallToolRequest>,
    ) -> Result<Response<McpCallToolResponse>, Status> {
        let request = request.into_inner();
        let tenant_id = parse_uuid(&request.tenant_id, "tenant_id")?;
        let server = server_ref(request.server)?;
        let arguments = std::str::from_utf8(&request.arguments_json)
            .map_err(|_| Status::invalid_argument("arguments_json is not utf-8"))?;
        let result = self
            .client
            .call_tool(
                tenant_id,
                &server,
                &request.qualified_name,
                arguments,
                &request.frozen_catalog_digest,
            )
            .await
            .map_err(to_status)?;
        Ok(Response::new(McpCallToolResponse {
            schema_version: SCHEMA_VERSION,
            content_json: result.content_json.into_bytes().into(),
            is_error: result.is_error,
        }))
    }
}

fn server_ref(
    server: Option<agent_model_gateway_protocol::v1::McpServerRef>,
) -> Result<McpServerRef, Status> {
    let server = server.ok_or_else(|| Status::invalid_argument("server is required"))?;
    Ok(McpServerRef {
        server_id: parse_uuid(&server.server_id, "server_id")?,
        name: server.name,
        endpoint: server.endpoint,
        credential_envelope_json: String::from_utf8(server.credential_envelope_json.to_vec())
            .map_err(|_| Status::invalid_argument("credential_envelope_json is not utf-8"))?,
    })
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(value).map_err(|_| Status::invalid_argument(format!("{field} is not a uuid")))
}

/// Maps failures to codes a caller can act on.
///
/// A refused call and an unreachable server are not the same event and must not
/// share a code: one means stop and tell the model, the other means the server
/// is down. Collapsing them is how a security refusal ends up being retried.
fn to_status(error: McpFederationError) -> Status {
    match error {
        McpFederationError::CatalogChanged
        | McpFederationError::ToolNotInFrozenCatalog(_)
        | McpFederationError::EndpointNotPermitted(_) => {
            Status::failed_precondition(error.to_string())
        }
        McpFederationError::CredentialUnopenable => Status::permission_denied(error.to_string()),
        McpFederationError::ResponseTooLarge => Status::out_of_range(error.to_string()),
        McpFederationError::Protocol(_) => Status::invalid_argument(error.to_string()),
        McpFederationError::Unreachable(_) => Status::unavailable(error.to_string()),
    }
}
