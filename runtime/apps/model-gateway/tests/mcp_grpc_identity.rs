//! The MCP federation RPCs must prove who is calling.
//!
//! The first version of this service read `tenant_id` out of the request body
//! and used it. Anything that could reach the port could name any tenant and
//! have the gateway open that tenant's sealed credential and call that tenant's
//! server. These tests exist so that cannot come back.

use agent_model_gateway::mcp::McpFederationClient;
use agent_model_gateway::mcp_grpc::McpFederationGrpcService;
use agent_model_gateway_protocol::mcp_server_authorization_digest;
use agent_model_gateway_protocol::v1::mcp_federation_client::McpFederationClient as WireClient;
use agent_model_gateway_protocol::v1::mcp_federation_server::McpFederationServer;
use agent_model_gateway_protocol::v1::{McpListToolsRequest, McpServerRef};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::rand_core::OsRng;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::Server;
use uuid::Uuid;

fn claims_for(tenant_id: Uuid, run_id: Uuid, scopes: &[&str]) -> WorkloadIdentityClaims {
    let now = chrono::Utc::now().timestamp_millis();
    WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id,
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id,
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
    // The signature covers "v2.<payload>", not the payload alone. Signing only
    // the payload produces a token that fails as InvalidSignature, which reads
    // as an authentication problem rather than a test that signed the wrong
    // bytes.
    let signing_input = format!("v2.{payload}");
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(signing_key.sign(signing_input.as_bytes()).to_bytes());
    format!("{signing_input}.{signature}")
}

async fn spawn_mcp_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16 * 1024];
                if socket.read(&mut buffer).await.unwrap_or(0) == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer).to_string();
                let body = if request.contains("\"tools/list\"") {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"web_search","description":"d","inputSchema":{"type":"object"}}]}}"#
                } else {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    format!("http://{address}/rpc")
}

async fn spawn_gateway(signing_key: &SigningKey) -> String {
    let pem = RsaPrivateKey::new(&mut OsRng, 3072)
        .unwrap()
        .to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .to_string();
    let federation =
        McpFederationClient::from_pkcs8_pem(&pem, Duration::from_secs(5), true).unwrap();
    let service = McpFederationGrpcService::new(
        federation,
        WorkloadTokenVerifier::new(signing_key.verifying_key()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .add_service(McpFederationServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .ok();
    });
    format!("http://{address}")
}

fn list_request(
    tenant_id: Uuid,
    run_id: Uuid,
    claims: &WorkloadIdentityClaims,
    endpoint: String,
) -> McpListToolsRequest {
    McpListToolsRequest {
        schema_version: 1,
        tenant_id: tenant_id.to_string(),
        run_id: run_id.to_string(),
        attempt_id: claims.attempt_id.to_string(),
        worker_id: claims.worker_id.to_string(),
        worker_incarnation_id: claims.worker_incarnation_id.to_string(),
        application_id: String::new(),
        workload_identity_id: String::new(),
        session_id: String::new(),
        workspace_id: String::new(),
        agent_version_id: String::new(),
        server: Some(McpServerRef {
            server_id: Uuid::now_v7().to_string(),
            name: "search".into(),
            endpoint,
            credential_envelope_json: Vec::new(),
            oauth_credential_id: String::new(),
            protocol_revision: "2025-06-18".into(),
            client_capabilities: Vec::new(),
        }),
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
async fn a_request_without_a_workload_token_is_refused() {
    let signing_key = SigningKey::from_bytes(&[41; 32]);
    let mcp = spawn_mcp_server().await;
    let gateway = spawn_gateway(&signing_key).await;
    let mut client = WireClient::connect(gateway).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), Uuid::now_v7(), &["mcp.federate"]);

    let status = client
        .list_tools(list_request(claims.tenant_id, claims.run_id, &claims, mcp))
        .await
        .expect_err("an unauthenticated request must be refused");
    assert_eq!(status.code(), Code::Unauthenticated);
}

/// The exact hole the first version had: a caller naming a tenant it holds no
/// token for. The body says one tenant, the signed token says another, and the
/// token wins.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_for_another_tenant_cannot_name_this_one() {
    let signing_key = SigningKey::from_bytes(&[42; 32]);
    let mcp = spawn_mcp_server().await;
    let gateway = spawn_gateway(&signing_key).await;
    let mut client = WireClient::connect(gateway).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), Uuid::now_v7(), &["mcp.federate"]);
    let token = sign(&signing_key, &claims);
    let victim = Uuid::now_v7();

    let status = client
        .list_tools(with_token(
            list_request(victim, claims.run_id, &claims, mcp),
            &token,
        ))
        .await
        .expect_err("a token issued for another tenant must not reach this one");
    assert_eq!(status.code(), Code::PermissionDenied);
}

/// A Run may hold a token and still not be entitled to federation. A token that
/// can execute a model is not automatically a token that can reach a tenant's
/// third-party servers.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_without_the_federation_scope_is_refused() {
    let signing_key = SigningKey::from_bytes(&[43; 32]);
    let mcp = spawn_mcp_server().await;
    let gateway = spawn_gateway(&signing_key).await;
    let mut client = WireClient::connect(gateway).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), Uuid::now_v7(), &["model.execute"]);
    let token = sign(&signing_key, &claims);

    let status = client
        .list_tools(with_token(
            list_request(claims.tenant_id, claims.run_id, &claims, mcp),
            &token,
        ))
        .await
        .expect_err("a token without mcp.federate must be refused");
    assert_eq!(status.code(), Code::Unauthenticated);
}

/// The run id is bound too, not just the tenant. A token for one Run must not
/// let a caller act as another Run of the same tenant.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_for_another_run_of_the_same_tenant_is_refused() {
    let signing_key = SigningKey::from_bytes(&[44; 32]);
    let mcp = spawn_mcp_server().await;
    let gateway = spawn_gateway(&signing_key).await;
    let mut client = WireClient::connect(gateway).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), Uuid::now_v7(), &["mcp.federate"]);
    let token = sign(&signing_key, &claims);

    let status = client
        .list_tools(with_token(
            list_request(claims.tenant_id, Uuid::now_v7(), &claims, mcp),
            &token,
        ))
        .await
        .expect_err("a token for another run must be refused");
    assert_eq!(status.code(), Code::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_correctly_bound_token_is_accepted() {
    let signing_key = SigningKey::from_bytes(&[45; 32]);
    let mcp = spawn_mcp_server().await;
    let gateway = spawn_gateway(&signing_key).await;
    let mut client = WireClient::connect(gateway).await.unwrap();
    let claims = claims_for(Uuid::now_v7(), Uuid::now_v7(), &["mcp.federate"]);
    let token = sign(&signing_key, &claims);

    let response = client
        .list_tools(with_token(
            list_request(claims.tenant_id, claims.run_id, &claims, mcp),
            &token,
        ))
        .await
        .expect("a correctly bound token should be accepted");
    assert_eq!(response.into_inner().tools.len(), 1);
}

/// The production break this catches is letting a valid Run token substitute
/// a different endpoint or credential envelope for an allowed MCP server ID.
#[tokio::test(flavor = "multi_thread")]
async fn a_v4_token_authorizes_only_the_exact_mcp_server_snapshot() {
    let signing_key = SigningKey::from_bytes(&[46; 32]);
    let mcp = spawn_mcp_server().await;
    let gateway = spawn_gateway(&signing_key).await;
    let mut client = WireClient::connect(gateway).await.unwrap();
    let mut claims = claims_for(Uuid::now_v7(), Uuid::now_v7(), &["mcp.federate"]);
    claims.schema_version = 4;
    claims.application_id = Uuid::now_v7();
    claims.workload_identity_id = Uuid::now_v7();
    claims.session_id = Uuid::now_v7();
    claims.workspace_id = Uuid::now_v7();
    claims.agent_version_id = Uuid::now_v7();
    claims.model_policy_digest = "a".repeat(64);
    let mut request = list_request(claims.tenant_id, claims.run_id, &claims, mcp);
    request.schema_version = 2;
    request.application_id = claims.application_id.to_string();
    request.workload_identity_id = claims.workload_identity_id.to_string();
    request.session_id = claims.session_id.to_string();
    request.workspace_id = claims.workspace_id.to_string();
    request.agent_version_id = claims.agent_version_id.to_string();
    let server = request.server.as_ref().unwrap();
    claims.authorized_mcp_servers.insert(
        Uuid::parse_str(&server.server_id).unwrap(),
        mcp_server_authorization_digest(server),
    );
    let token = sign(&signing_key, &claims);

    client
        .list_tools(with_token(request.clone(), &token))
        .await
        .expect("the exact signed MCP server snapshot should be accepted");

    request.server.as_mut().unwrap().endpoint = "http://127.0.0.1:9/substituted".into();
    let status = client
        .list_tools(with_token(request, &token))
        .await
        .expect_err("a substituted MCP snapshot must be refused before egress");
    assert_eq!(status.code(), Code::PermissionDenied);
}
