//! The OAuth admin RPCs must prove who is calling, and must never hand back
//! credential material.
//!
//! Administering a credential is a strictly larger power than using one: a
//! caller who can revoke can lock a tenant out, and a caller who can begin an
//! authorization can point a tenant at an authorization server. These tests
//! exist so that power cannot be reached with a federation token, with another
//! tenant's token, or without a token at all.

use agent_model_gateway::mcp_oauth::McpOAuthCoordinator;
use agent_model_gateway::mcp_oauth_grpc::McpOAuthAdminGrpcService;
use agent_model_gateway_protocol::v1::mcp_oauth_admin_client::McpOauthAdminClient as WireClient;
use agent_model_gateway_protocol::v1::mcp_oauth_admin_server::McpOauthAdminServer;
use agent_model_gateway_protocol::v1::{
    McpOauthAdminContext, McpOauthCredentialRef, McpOauthRevokeRequest, McpOauthStatusRequest,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::Server;
use uuid::Uuid;

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("agent-mcp-oauth-admin-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Note the run, attempt and worker identifiers: the workload token format has
/// no operator shape, and `verify` refuses claims whose run/attempt/worker are
/// nil. So an administrative token today is a Run-bound token carrying an extra
/// scope, and the separation between federating and administering rests on that
/// scope rather than on a distinct identity shape.
fn claims_for(tenant_id: Uuid, scopes: &[&str]) -> WorkloadIdentityClaims {
    let now = chrono::Utc::now().timestamp_millis();
    WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id,
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: Uuid::now_v7(),
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
        model_policy_digest: String::new(),
        authorized_mcp_servers: Default::default(),
        audiences: BTreeSet::from(["model-gateway".to_owned()]),
        scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + 60_000,
    }
}

fn sign(signing_key: &SigningKey, claims: &WorkloadIdentityClaims) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(signing_key.sign(signing_input.as_bytes()).to_bytes());
    format!("{signing_input}.{signature}")
}

async fn spawn_admin(signing_key: &SigningKey, root: &TestRoot) -> String {
    let coordinator =
        McpOAuthCoordinator::new(&root.0, [23_u8; 32], Duration::from_secs(5), true).unwrap();
    let service = McpOAuthAdminGrpcService::new(
        Arc::new(coordinator),
        WorkloadTokenVerifier::new(signing_key.verifying_key()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .add_service(McpOauthAdminServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .ok();
    });
    format!("http://{address}")
}

fn context(claims: &WorkloadIdentityClaims) -> McpOauthAdminContext {
    McpOauthAdminContext {
        schema_version: 1,
        tenant_id: claims.tenant_id.to_string(),
        application_id: claims.application_id.to_string(),
        workload_identity_id: claims.workload_identity_id.to_string(),
    }
}

fn credential() -> McpOauthCredentialRef {
    McpOauthCredentialRef {
        server_id: Uuid::now_v7().to_string(),
        // A literal keeps binding validation satisfied without depending on
        // external DNS; these tests are about identity, not reachability.
        endpoint: "http://127.0.0.1:9/mcp".to_owned(),
        credential_id: Uuid::now_v7().to_string(),
    }
}

fn with_token<T>(message: T, token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        tonic::metadata::MetadataValue::try_from(format!("Bearer {token}")).unwrap(),
    );
    request
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_request_without_a_workload_token_is_refused() {
    let signing_key = SigningKey::from_bytes(&[71; 32]);
    let root = TestRoot::new();
    let admin = spawn_admin(&signing_key, &root).await;
    let mut client = WireClient::connect(admin).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), &["mcp.oauth.admin"]);

    let status = client
        .get_credential_status(McpOauthStatusRequest {
            context: Some(context(&claims)),
            credential: Some(credential()),
        })
        .await
        .expect_err("an unauthenticated admin request must be refused");
    assert_eq!(status.code(), Code::Unauthenticated);
}

/// The separation that matters: federating is not administering. A Worker token
/// that may call a tenant's tools must not be able to revoke that tenant's
/// grant.
#[tokio::test(flavor = "multi_thread")]
async fn a_federation_token_cannot_administer_credentials() {
    let signing_key = SigningKey::from_bytes(&[72; 32]);
    let root = TestRoot::new();
    let admin = spawn_admin(&signing_key, &root).await;
    let mut client = WireClient::connect(admin).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), &["mcp.federate"]);
    let token = sign(&signing_key, &claims);

    let status = client
        .revoke(with_token(
            McpOauthRevokeRequest {
                context: Some(context(&claims)),
                credential: Some(credential()),
            },
            &token,
        ))
        .await
        .expect_err("a federation-scoped token must not reach the admin surface");
    assert!(
        matches!(
            status.code(),
            Code::Unauthenticated | Code::PermissionDenied
        ),
        "unexpected code {:?}",
        status.code()
    );
}

/// A token issued for one tenant must not administer another's credential, even
/// though the body is the only place the tenant is named.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_for_another_tenant_cannot_administer_it() {
    let signing_key = SigningKey::from_bytes(&[73; 32]);
    let root = TestRoot::new();
    let admin = spawn_admin(&signing_key, &root).await;
    let mut client = WireClient::connect(admin).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), &["mcp.oauth.admin"]);
    let token = sign(&signing_key, &claims);
    let mut foreign = context(&claims);
    foreign.tenant_id = Uuid::now_v7().to_string();

    let status = client
        .get_credential_status(with_token(
            McpOauthStatusRequest {
                context: Some(foreign),
                credential: Some(credential()),
            },
            &token,
        ))
        .await
        .expect_err("a token issued for another tenant must not reach this one");
    assert_eq!(status.code(), Code::PermissionDenied);
}

/// An authorized caller gets a status and nothing else. A credential that was
/// never authorized reads as absent rather than as an error, and the response
/// carries no field that could hold credential material.
#[tokio::test(flavor = "multi_thread")]
async fn an_authorized_status_call_returns_no_credential_material() {
    let signing_key = SigningKey::from_bytes(&[74; 32]);
    let root = TestRoot::new();
    let admin = spawn_admin(&signing_key, &root).await;
    let mut client = WireClient::connect(admin).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), &["mcp.oauth.admin"]);
    let token = sign(&signing_key, &claims);

    let response = client
        .get_credential_status(with_token(
            McpOauthStatusRequest {
                context: Some(context(&claims)),
                credential: Some(credential()),
            },
            &token,
        ))
        .await
        .expect("an authorized admin call must succeed")
        .into_inner();
    assert_eq!(response.status, "absent");
    assert_eq!(response.revision, 0);
    assert!(response.reason.is_empty());
    // The whole wire message, not just the fields this test names: if a future
    // field ever carried a token, this catches it.
    let rendered = format!("{response:?}").to_ascii_lowercase();
    for forbidden in ["access_token", "refresh_token", "verifier", "code_verifier"] {
        assert!(
            !rendered.contains(forbidden),
            "status response leaked {forbidden}: {rendered}"
        );
    }
}
