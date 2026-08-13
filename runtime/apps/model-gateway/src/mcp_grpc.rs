//! gRPC surface for federated MCP calls (ADR-0040).
//!
//! Thin on purpose. The rules live in [`crate::mcp`] -- endpoint restriction,
//! catalog freezing, response bounds, credential opening -- and this layer only
//! translates. A rule implemented here instead would be a rule the library tests
//! could not reach.

use crate::mcp::{
    McpFederationClient, McpFederationError, McpRoundTripContinuation, McpServerRef,
    McpToolCallOutcome,
};
use agent_model_gateway_protocol::mcp_server_authorization_digest;
use agent_model_gateway_protocol::v1::mcp_federation_server::McpFederation;
use agent_model_gateway_protocol::v1::{
    McpCallToolRequest, McpCallToolResponse, McpListToolsRequest, McpListToolsResponse, McpTool,
};
use agent_protocol::{McpClientCapability, McpProtocolRevision};
use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadIdentityClaims, WorkloadTokenVerifier,
};
use chrono::Utc;
use std::collections::BTreeSet;
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
            application_id: asserted.application_id,
            workload_identity_id: asserted.workload_identity_id,
            run_id: asserted.run_id,
            session_id: asserted.session_id,
            workspace_id: asserted.workspace_id,
            agent_version_id: asserted.agent_version_id,
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
    application_id: Uuid,
    workload_identity_id: Uuid,
    run_id: Uuid,
    session_id: Uuid,
    workspace_id: Uuid,
    agent_version_id: Uuid,
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
            application_id: parse_optional_uuid(
                &request.get_ref().application_id,
                "application_id",
            )?,
            workload_identity_id: parse_optional_uuid(
                &request.get_ref().workload_identity_id,
                "workload_identity_id",
            )?,
            run_id: parse_uuid(&request.get_ref().run_id, "run_id")?,
            session_id: parse_optional_uuid(&request.get_ref().session_id, "session_id")?,
            workspace_id: parse_optional_uuid(&request.get_ref().workspace_id, "workspace_id")?,
            agent_version_id: parse_optional_uuid(
                &request.get_ref().agent_version_id,
                "agent_version_id",
            )?,
            attempt_id: parse_uuid(&request.get_ref().attempt_id, "attempt_id")?,
            worker_id: parse_uuid(&request.get_ref().worker_id, "worker_id")?,
            worker_incarnation_id: parse_uuid(
                &request.get_ref().worker_incarnation_id,
                "worker_incarnation_id",
            )?,
        };
        let claims = self.authenticate(&request, &asserted)?;
        let request = request.into_inner();
        let wire_server = request
            .server
            .ok_or_else(|| Status::invalid_argument("server is required"))?;
        authorize_server(request.schema_version, &claims, &wire_server)?;
        let server = server_ref(wire_server)?;
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
                    input_schema_json: tool.input_schema_json.into_bytes(),
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
            application_id: parse_optional_uuid(
                &request.get_ref().application_id,
                "application_id",
            )?,
            workload_identity_id: parse_optional_uuid(
                &request.get_ref().workload_identity_id,
                "workload_identity_id",
            )?,
            run_id: parse_uuid(&request.get_ref().run_id, "run_id")?,
            session_id: parse_optional_uuid(&request.get_ref().session_id, "session_id")?,
            workspace_id: parse_optional_uuid(&request.get_ref().workspace_id, "workspace_id")?,
            agent_version_id: parse_optional_uuid(
                &request.get_ref().agent_version_id,
                "agent_version_id",
            )?,
            attempt_id: parse_uuid(&request.get_ref().attempt_id, "attempt_id")?,
            worker_id: parse_uuid(&request.get_ref().worker_id, "worker_id")?,
            worker_incarnation_id: parse_uuid(
                &request.get_ref().worker_incarnation_id,
                "worker_incarnation_id",
            )?,
        };
        let claims = self.authenticate(&request, &asserted)?;
        let request = request.into_inner();
        let wire_server = request
            .server
            .ok_or_else(|| Status::invalid_argument("server is required"))?;
        authorize_server(request.schema_version, &claims, &wire_server)?;
        let server = server_ref(wire_server)?;
        let arguments = std::str::from_utf8(&request.arguments_json)
            .map_err(|_| Status::invalid_argument("arguments_json is not utf-8"))?;
        let continuation = if request.input_continuation_json.is_empty() {
            None
        } else {
            Some(
                serde_json::from_slice::<McpRoundTripContinuation>(
                    &request.input_continuation_json,
                )
                .map_err(|_| Status::invalid_argument("input_continuation_json is malformed"))?,
            )
        };
        let outcome = self
            .client
            .call_tool_round(
                claims.tenant_id,
                &server,
                &request.qualified_name,
                arguments,
                &request.frozen_catalog_digest,
                continuation.as_ref(),
            )
            .await
            .map_err(to_status)?;
        match outcome {
            McpToolCallOutcome::Complete(result) => Ok(Response::new(McpCallToolResponse {
                schema_version: SCHEMA_VERSION,
                content_json: result.content_json.into_bytes(),
                is_error: result.is_error,
                input_required_json: Vec::new(),
            })),
            McpToolCallOutcome::InputRequired(required) => {
                let input_required_json = serde_json::to_vec(&required).map_err(|_| {
                    Status::internal("MCP input-required result could not be encoded")
                })?;
                Ok(Response::new(McpCallToolResponse {
                    schema_version: SCHEMA_VERSION,
                    content_json: Vec::new(),
                    is_error: false,
                    input_required_json,
                }))
            }
        }
    }
}

fn authorize_server(
    schema_version: u32,
    claims: &WorkloadIdentityClaims,
    server: &agent_model_gateway_protocol::v1::McpServerRef,
) -> Result<(), Status> {
    match schema_version {
        1 if matches!(claims.schema_version, 2 | 3) => Ok(()),
        2 if claims.schema_version == 4 => {
            let server_id = parse_uuid(&server.server_id, "server_id")?;
            let digest = mcp_server_authorization_digest(server);
            if claims.authorized_mcp_servers.get(&server_id) == Some(&digest) {
                Ok(())
            } else {
                Err(Status::permission_denied(
                    "workload token does not authorize this MCP server snapshot",
                ))
            }
        }
        _ => Err(Status::permission_denied(
            "workload token schema does not authorize this MCP request schema",
        )),
    }
}

fn server_ref(
    server: agent_model_gateway_protocol::v1::McpServerRef,
) -> Result<McpServerRef, Status> {
    let protocol_revision = match server.protocol_revision.as_str() {
        "" | "2025-06-18" => McpProtocolRevision::V2025_06_18,
        "2026-07-28" => McpProtocolRevision::V2026_07_28,
        revision => {
            return Err(Status::invalid_argument(format!(
                "unsupported MCP protocol revision {revision}"
            )));
        }
    };
    let client_capabilities = server
        .client_capabilities
        .into_iter()
        .map(|capability| match capability.as_str() {
            "elicitation" => Ok(McpClientCapability::Elicitation),
            _ => Err(Status::invalid_argument(
                "unsupported MCP client capability",
            )),
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if protocol_revision == McpProtocolRevision::V2025_06_18 && !client_capabilities.is_empty() {
        return Err(Status::invalid_argument(
            "legacy MCP servers cannot carry modern client capabilities",
        ));
    }
    Ok(McpServerRef {
        server_id: parse_uuid(&server.server_id, "server_id")?,
        name: server.name,
        endpoint: server.endpoint,
        credential_envelope_json: String::from_utf8(server.credential_envelope_json.to_vec())
            .map_err(|_| Status::invalid_argument("credential_envelope_json is not utf-8"))?,
        protocol_revision,
        client_capabilities,
    })
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(value).map_err(|_| Status::invalid_argument(format!("{field} is not a uuid")))
}

fn parse_optional_uuid(value: &str, field: &str) -> Result<Uuid, Status> {
    if value.is_empty() {
        Ok(Uuid::nil())
    } else {
        parse_uuid(value, field)
    }
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
        McpFederationError::Cancelled => Status::cancelled(error.to_string()),
    }
}
