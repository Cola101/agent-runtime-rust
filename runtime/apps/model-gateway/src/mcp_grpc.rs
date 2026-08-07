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
use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadIdentityClaims, WorkloadTokenVerifier,
};
use chrono::Utc;
use tonic::{Request, Response, Status};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;

/// Federation is its own capability.
///
/// A token that can execute a model is not automatically a token that may reach
/// a tenant's third-party servers and have their sealed credential opened. Two
/// scopes means a future policy can withhold one without withholding the other;
/// one scope means it never can.
const FEDERATION_SCOPE: &str = "mcp.federate";

pub struct McpFederationGrpcService {
    client: McpFederationClient,
    verifier: WorkloadTokenVerifier,
}

impl McpFederationGrpcService {
    pub fn new(client: McpFederationClient, verifier: WorkloadTokenVerifier) -> Self {
        Self { client, verifier }
    }

    /// Verifies the bearer token and binds it to the identity the request
    /// asserts.
    ///
    /// The first version of this service took `tenant_id` from the request body
    /// and used it, so anything that could reach the port could name any tenant
    /// and have that tenant's credential opened. The claims are the authority
    /// now: the body must agree with them, and the values used downstream come
    /// from the token.
    fn authenticate<T>(
        &self,
        request: &Request<T>,
        asserted: &AssertedIdentity,
    ) -> Result<WorkloadIdentityClaims, Status> {
        let bearer = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| Status::unauthenticated("missing workload bearer token"))?;
        let claims = self
            .verifier
            .verify(
                bearer,
                RequiredCapability::new("model-gateway", FEDERATION_SCOPE, true),
                Utc::now().timestamp_millis(),
            )
            .map_err(|_| Status::unauthenticated("invalid workload token"))?;
        let binding = WorkloadIdentityBinding {
            tenant_id: asserted.tenant_id,
            run_id: asserted.run_id,
            attempt_id: asserted.attempt_id,
            worker_id: asserted.worker_id,
            worker_incarnation_id: asserted.worker_incarnation_id,
        };
        if !claims.authorizes(&binding) {
            // Deliberately not Unauthenticated: the token is valid, and the
            // caller is asking to act as an identity it does not hold. Saying
            // "authenticate again" would send it round a loop that cannot help.
            return Err(Status::permission_denied(
                "workload token does not authorize this tenant, run, attempt or worker",
            ));
        }
        Ok(claims)
    }
}

/// What the request says about itself, before anything has verified it.
struct AssertedIdentity {
    tenant_id: Uuid,
    run_id: Uuid,
    attempt_id: Uuid,
    worker_id: Uuid,
    worker_incarnation_id: Uuid,
}

#[tonic::async_trait]
impl McpFederation for McpFederationGrpcService {
    async fn list_tools(
        &self,
        request: Request<McpListToolsRequest>,
    ) -> Result<Response<McpListToolsResponse>, Status> {
        let asserted = AssertedIdentity {
            tenant_id: parse_uuid(&request.get_ref().tenant_id, "tenant_id")?,
            run_id: parse_uuid(&request.get_ref().run_id, "run_id")?,
            attempt_id: parse_uuid(&request.get_ref().attempt_id, "attempt_id")?,
            worker_id: parse_uuid(&request.get_ref().worker_id, "worker_id")?,
            worker_incarnation_id: parse_uuid(
                &request.get_ref().worker_incarnation_id,
                "worker_incarnation_id",
            )?,
        };
        let claims = self.authenticate(&request, &asserted)?;
        let request = request.into_inner();
        let server = server_ref(request.server)?;
        // From the claims, not the body. If the two ever diverge the signed one
        // is the one that means something.
        let catalog = self
            .client
            .list_tools(claims.tenant_id, &server)
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
        let asserted = AssertedIdentity {
            tenant_id: parse_uuid(&request.get_ref().tenant_id, "tenant_id")?,
            run_id: parse_uuid(&request.get_ref().run_id, "run_id")?,
            attempt_id: parse_uuid(&request.get_ref().attempt_id, "attempt_id")?,
            worker_id: parse_uuid(&request.get_ref().worker_id, "worker_id")?,
            worker_incarnation_id: parse_uuid(
                &request.get_ref().worker_incarnation_id,
                "worker_incarnation_id",
            )?,
        };
        let claims = self.authenticate(&request, &asserted)?;
        let request = request.into_inner();
        let server = server_ref(request.server)?;
        let arguments = std::str::from_utf8(&request.arguments_json)
            .map_err(|_| Status::invalid_argument("arguments_json is not utf-8"))?;
        let result = self
            .client
            .call_tool(
                claims.tenant_id,
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
