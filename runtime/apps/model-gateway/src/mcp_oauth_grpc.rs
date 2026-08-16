//! gRPC surface for administering MCP OAuth credentials (ADR-0120).
//!
//! Thin on purpose, like [`crate::mcp_grpc`]. Discovery bounds, PKCE, the CAS
//! state machine and revocation ordering all live in [`crate::mcp_oauth`]; this
//! layer only translates and authenticates. A rule implemented here instead
//! would be a rule the coordinator's own tests could not reach.
//!
//! Nothing this service returns carries an access token, refresh token,
//! authorization code, PKCE verifier or OAuth state.

use crate::mcp_oauth::{
    McpOAuthAuthorizationReason, McpOAuthBinding, McpOAuthClientConfig, McpOAuthCoordinator,
    McpOAuthCredentialStatus, McpOAuthError,
};
use agent_model_gateway_protocol::v1::mcp_oauth_admin_server::McpOauthAdmin;
use agent_model_gateway_protocol::v1::{
    McpOauthAdminContext, McpOauthBeginRequest, McpOauthBeginResponse, McpOauthCompleteRequest,
    McpOauthCompleteResponse, McpOauthCredentialRef, McpOauthRevokeRequest, McpOauthRevokeResponse,
    McpOauthStatusRequest, McpOauthStatusResponse,
};
use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadIdentityClaims, WorkloadTokenVerifier,
};
use chrono::Utc;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;

/// Administering credentials is its own capability.
///
/// A token that may call a tenant's MCP tools is not automatically a token that
/// may mint or destroy that tenant's OAuth grants. Reusing `mcp.federate` here
/// would mean every Worker that can federate can also revoke, and no later
/// policy could separate them.
const OAUTH_ADMIN_SCOPE: &str = "mcp.oauth.admin";

pub struct McpOAuthAdminGrpcService {
    coordinator: Arc<McpOAuthCoordinator>,
    verifier: WorkloadTokenVerifier,
}

impl McpOAuthAdminGrpcService {
    #[must_use]
    pub fn new(coordinator: Arc<McpOAuthCoordinator>, verifier: WorkloadTokenVerifier) -> Self {
        Self {
            coordinator,
            verifier,
        }
    }

    /// Verifies the bearer token and binds it to the identity the request
    /// asserts.
    ///
    /// An administrative caller is not a Run, so the body carries only tenant,
    /// application and workload identity. The run-scoped fields are taken from
    /// the claims rather than from the request: that way the body can never
    /// widen what the token already says, while a mismatch on the three fields
    /// it does name still fails.
    fn authenticate<T>(
        &self,
        request: &Request<T>,
        context: Option<&McpOauthAdminContext>,
    ) -> Result<WorkloadIdentityClaims, Status> {
        let context = context.ok_or_else(|| Status::invalid_argument("missing admin context"))?;
        if context.schema_version != SCHEMA_VERSION {
            return Err(Status::invalid_argument(
                "unsupported mcp oauth admin schema version",
            ));
        }
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
                RequiredCapability::new("model-gateway", OAUTH_ADMIN_SCOPE, true),
                Utc::now().timestamp_millis(),
            )
            .map_err(|_| Status::unauthenticated("invalid workload token"))?;
        let binding = WorkloadIdentityBinding {
            tenant_id: parse_uuid(&context.tenant_id, "tenant_id")?,
            application_id: parse_uuid(&context.application_id, "application_id")?,
            workload_identity_id: parse_uuid(
                &context.workload_identity_id,
                "workload_identity_id",
            )?,
            run_id: claims.run_id,
            session_id: claims.session_id,
            workspace_id: claims.workspace_id,
            agent_version_id: claims.agent_version_id,
            attempt_id: claims.attempt_id,
            worker_id: claims.worker_id,
            worker_incarnation_id: claims.worker_incarnation_id,
        };
        if !claims.authorizes(&binding) {
            // Deliberately not Unauthenticated: the token is valid and the
            // caller is asking to act as an identity it does not hold. Telling
            // it to authenticate again would send it round a loop that cannot
            // help.
            return Err(Status::permission_denied(
                "workload token does not authorize this tenant, application or workload identity",
            ));
        }
        Ok(claims)
    }

    /// Builds the credential binding from claims-verified identity.
    ///
    /// The tenant comes from the verified claims rather than the body, so a
    /// caller cannot name someone else's tenant and have their credential
    /// resolved. The other three fields are pinned into the stored record's AAD,
    /// so a mismatched server or endpoint resolves to nothing rather than to a
    /// different tenant's token.
    fn binding(
        claims: &WorkloadIdentityClaims,
        credential: Option<&McpOauthCredentialRef>,
    ) -> Result<McpOAuthBinding, Status> {
        let credential =
            credential.ok_or_else(|| Status::invalid_argument("missing credential reference"))?;
        Ok(McpOAuthBinding {
            tenant_id: claims.tenant_id,
            server_id: parse_uuid(&credential.server_id, "server_id")?,
            credential_id: parse_uuid(&credential.credential_id, "credential_id")?,
            endpoint: credential.endpoint.clone(),
        })
    }
}

#[tonic::async_trait]
impl McpOauthAdmin for McpOAuthAdminGrpcService {
    async fn begin_authorization(
        &self,
        request: Request<McpOauthBeginRequest>,
    ) -> Result<Response<McpOauthBeginResponse>, Status> {
        let claims = self.authenticate(&request, request.get_ref().context.as_ref())?;
        let message = request.get_ref();
        let binding = Self::binding(&claims, message.credential.as_ref())?;
        let client = McpOAuthClientConfig {
            client_id: message.client_id.clone(),
            redirect_uri: message.redirect_uri.clone(),
            requested_scopes: message.requested_scopes.clone(),
        };
        let challenge = Some(message.www_authenticate.as_str()).filter(|value| !value.is_empty());
        let start = self
            .coordinator
            .begin_discovered_authorization(binding, client, challenge, Utc::now())
            .await
            .map_err(map_status)?;
        Ok(Response::new(McpOauthBeginResponse {
            flow_id: start.flow_id.to_string(),
            authorization_url: start.authorization_url,
            expires_at_ms: start.expires_at.timestamp_millis(),
        }))
    }

    async fn complete_authorization(
        &self,
        request: Request<McpOauthCompleteRequest>,
    ) -> Result<Response<McpOauthCompleteResponse>, Status> {
        let claims = self.authenticate(&request, request.get_ref().context.as_ref())?;
        let message = request.get_ref();
        let binding = Self::binding(&claims, message.credential.as_ref())?;
        let flow_id = parse_uuid(&message.flow_id, "flow_id")?;
        // The resolved credential is dropped here on purpose: the caller learns
        // that the exchange committed and at which revision, never the token.
        let resolved = self
            .coordinator
            .complete_authorization(
                binding.clone(),
                flow_id,
                &message.state,
                &message.authorization_code,
                Utc::now(),
            )
            .await
            .map_err(map_status)?;
        let revision = resolved.revision();
        drop(resolved);
        Ok(Response::new(McpOauthCompleteResponse {
            status: "active".to_owned(),
            revision,
        }))
    }

    async fn get_credential_status(
        &self,
        request: Request<McpOauthStatusRequest>,
    ) -> Result<Response<McpOauthStatusResponse>, Status> {
        let claims = self.authenticate(&request, request.get_ref().context.as_ref())?;
        let binding = Self::binding(&claims, request.get_ref().credential.as_ref())?;
        let status = self.coordinator.status(binding).await.map_err(map_status)?;
        Ok(Response::new(status_response(&status)))
    }

    async fn revoke(
        &self,
        request: Request<McpOauthRevokeRequest>,
    ) -> Result<Response<McpOauthRevokeResponse>, Status> {
        let claims = self.authenticate(&request, request.get_ref().context.as_ref())?;
        let binding = Self::binding(&claims, request.get_ref().credential.as_ref())?;
        let outcome = self.coordinator.revoke(binding).await.map_err(map_status)?;
        Ok(Response::new(McpOauthRevokeResponse {
            revision: outcome.revision,
            remote_confirmed: outcome.remote_confirmed,
        }))
    }
}

fn parse_uuid(value: &str, field: &'static str) -> Result<Uuid, Status> {
    Uuid::parse_str(value).map_err(|_| Status::invalid_argument(format!("{field} must be a UUID")))
}

fn status_response(status: &McpOAuthCredentialStatus) -> McpOauthStatusResponse {
    let (label, revision, reason, expires_at_ms) = match status {
        McpOAuthCredentialStatus::Absent => ("absent", 0, "", 0),
        McpOAuthCredentialStatus::PendingAuthorization {
            expires_at,
            revision,
        } => (
            "pending_authorization",
            *revision,
            "",
            expires_at.timestamp_millis(),
        ),
        McpOAuthCredentialStatus::Active {
            expires_at,
            revision,
        } => (
            "active",
            *revision,
            "",
            expires_at.map_or(0, |at| at.timestamp_millis()),
        ),
        McpOAuthCredentialStatus::AuthorizationRequired { reason, revision } => (
            "authorization_required",
            *revision,
            reason_label(*reason),
            0,
        ),
        McpOAuthCredentialStatus::Revoked { revision } => ("revoked", *revision, "", 0),
    };
    McpOauthStatusResponse {
        status: label.to_owned(),
        revision,
        reason: reason.to_owned(),
        expires_at_ms,
    }
}

fn reason_label(reason: McpOAuthAuthorizationReason) -> &'static str {
    match reason {
        McpOAuthAuthorizationReason::Missing => "missing",
        McpOAuthAuthorizationReason::FlowExpired => "flow_expired",
        McpOAuthAuthorizationReason::ExchangeIndeterminate => "exchange_indeterminate",
        McpOAuthAuthorizationReason::RefreshIndeterminate => "refresh_indeterminate",
        McpOAuthAuthorizationReason::ProviderRejected => "provider_rejected",
        McpOAuthAuthorizationReason::AccessTokenRejected => "access_token_rejected",
        McpOAuthAuthorizationReason::NoRefreshToken => "no_refresh_token",
        McpOAuthAuthorizationReason::Revoked => "revoked",
    }
}

/// Maps credential-domain errors to transport status.
///
/// Discovery and provider rejections collapse into one status on purpose. Which
/// specific check tripped -- resource mismatch, issuer disagreement, a field
/// over its cap -- is exactly what a prober would want to learn, and the
/// coordinator already keeps its own error coarse for the same reason.
fn map_status(error: McpOAuthError) -> Status {
    match error {
        McpOAuthError::InvalidBinding | McpOAuthError::InvalidAuthorizationRequest => {
            Status::invalid_argument("mcp oauth request is invalid")
        }
        McpOAuthError::InvalidAuthorizationCallback => {
            Status::failed_precondition("mcp oauth authorization callback is invalid or stale")
        }
        McpOAuthError::AuthorizationRequired => {
            Status::failed_precondition("mcp oauth authorization is required")
        }
        McpOAuthError::DiscoveryRejected | McpOAuthError::ProviderRejected => {
            Status::failed_precondition("mcp oauth discovery or provider was rejected")
        }
        McpOAuthError::StoreUnavailable | McpOAuthError::ProviderUnavailable => {
            Status::unavailable("mcp oauth credential domain is unavailable")
        }
    }
}
