//! The invocation surface over real mTLS.
//!
//! Until this file existed, transport security was *configured* and never
//! *exercised*: the configuration tests wrote invalid certificate files and
//! asserted the process would not start, and the end-to-end test used a
//! plaintext loopback server. Nothing had ever crossed TLS in either direction.
//!
//! Proving the happy path alone would not be enough. A server that accepts a
//! client presenting no certificate, or one signed by an unrelated authority,
//! is running TLS -- not mutual TLS. The refusals are what the "m" means, so
//! they are asserted here beside the success.

use agent_grpc_security::{ClientMtlsMaterials, ServerMtlsMaterials};
use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::grpc::RuntimeInvocationGrpcService;
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent,
};
use agent_runtime_invocation_protocol::v1::run_lifecycle_boundary::Boundary;
use agent_runtime_invocation_protocol::v1::runtime_invocation_client::RuntimeInvocationClient;
use agent_runtime_invocation_protocol::v1::runtime_invocation_server::RuntimeInvocationServer;
use agent_runtime_invocation_protocol::v1::{
    ReadRunEventsRequest, RuntimeInvocationRef, SubmitRunRequest,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Server};
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";
const SERVER_DOMAIN: &str = "runtime-host.test";
const MODEL_REPLY: &str = "answered over mutual tls";

struct TestPki {
    server: ServerMtlsMaterials,
    client: ClientMtlsMaterials,
    /// A second, equally valid client.
    ///
    /// The refusal tests use it as a control: a refusal only means something if
    /// a good certificate reaches the same server. Without it, a server that
    /// failed to start would make every refusal test pass.
    control_client: ClientMtlsMaterials,
    /// A client trusted by nobody the server knows.
    foreign_client: ClientMtlsMaterials,
    ca_pem: Vec<u8>,
}

fn issuer() -> (Issuer<'static, KeyPair>, Vec<u8>) {
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let key = KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let pem = certificate.pem().into_bytes();
    (Issuer::new(params, key), pem)
}

fn client_materials(issuer: &Issuer<'static, KeyPair>, ca_pem: Vec<u8>) -> ClientMtlsMaterials {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["runtime-caller.test".into()]).unwrap();
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let certificate = params.signed_by(&key, issuer).unwrap();
    ClientMtlsMaterials::new(
        certificate.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
        ca_pem,
        SERVER_DOMAIN.into(),
    )
    .unwrap()
}

fn test_pki() -> TestPki {
    let (authority, ca_pem) = issuer();
    let server_key = KeyPair::generate().unwrap();
    let mut server_params = CertificateParams::new(vec![SERVER_DOMAIN.into()]).unwrap();
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_certificate = server_params.signed_by(&server_key, &authority).unwrap();

    // A second, unrelated authority. Its client is well-formed and correctly
    // signed -- just not by anyone this server trusts.
    let (foreign_authority, _) = issuer();

    TestPki {
        server: ServerMtlsMaterials::new(
            server_certificate.pem().into_bytes(),
            server_key.serialize_pem().into_bytes(),
            ca_pem.clone(),
        )
        .unwrap(),
        client: client_materials(&authority, ca_pem.clone()),
        control_client: client_materials(&authority, ca_pem.clone()),
        foreign_client: client_materials(&foreign_authority, ca_pem.clone()),
        ca_pem,
    }
}

async fn spawn_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            let body = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{MODEL_REPLY}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    endpoint
}

fn operator_claims(tenant_id: Uuid) -> WorkloadIdentityClaims {
    let now = chrono::Utc::now().timestamp_millis();
    WorkloadIdentityClaims {
        schema_version: agent_workload_identity::OPERATOR_SCHEMA_VERSION,
        tenant_id,
        application_id: Uuid::now_v7(),
        workload_identity_id: Uuid::now_v7(),
        run_id: Uuid::nil(),
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: Uuid::nil(),
        worker_id: Uuid::nil(),
        worker_incarnation_id: Uuid::nil(),
        model_policy_id: Uuid::nil(),
        model_policy_digest: String::new(),
        authorized_mcp_servers: Default::default(),
        audiences: BTreeSet::from(["runtime-host".to_owned()]),
        scopes: BTreeSet::from([INVOKE_SCOPE.to_owned()]),
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

fn with_token<T>(message: T, token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        tonic::metadata::MetadataValue::try_from(format!("Bearer {token}")).unwrap(),
    );
    request
}

fn config(
    state: &tempfile::TempDir,
    workspace: &tempfile::TempDir,
    provider_endpoint: String,
) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root: state.path().to_path_buf(),
        workspace_root: workspace.path().to_path_buf(),
        agent_instructions: "Answer briefly.".into(),
        delegated_scopes: BTreeSet::new(),
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig {
            allowed_regions: BTreeSet::from(["local".into()]),
            data_class: DataClass::Internal,
            max_cost_per_million_tokens_micros: 1_000_000,
            health_policy: Default::default(),
            candidates: vec![LocalProviderConfig {
                id: "loopback".into(),
                protocol: ProviderProtocol::OpenAiCompatible,
                endpoint: provider_endpoint,
                model: "test-model".into(),
                api_key: "test-key".into(),
                region: "local".into(),
                accepted_data_classes: BTreeSet::from([DataClass::Internal]),
                capabilities: BTreeSet::from([Capability::Text]),
                healthy: true,
                latency_ms: 1,
                cost_per_million_tokens_micros: 1,
                response_timeout_ms: 5_000,
                stream_idle_timeout_ms: 5_000,
            }],
        },
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 1_000,
            max_cost_cents: 100,
            max_duration_seconds: 60,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

/// Serves the surface over real TLS and returns where it is listening.
async fn spawn_tls_surface(
    signing_key: &SigningKey,
    profile: RuntimeInvocationContext,
    config: LocalRuntimeConfig,
    server_tls: ServerMtlsMaterials,
) -> SocketAddr {
    let runtime = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 2,
            max_active_runs_per_tenant: 2,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 8,
            max_queued_runs_per_tenant: 4,
        },
        vec![RuntimeProfile {
            invocation: profile,
            config,
        }],
    )
    .unwrap();
    let service = RuntimeInvocationGrpcService::new(
        Arc::new(runtime),
        WorkloadTokenVerifier::new(signing_key.verifying_key()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .tls_config(server_tls.into_tonic())
            .unwrap()
            .add_service(RuntimeInvocationServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .ok();
    });
    address
}

/// `127.0.0.1` is dialled while the certificate is verified against
/// `runtime-host.test`, which is what `domain_name` on the client config is
/// for. Without it this would be testing a hostname, not a certificate.
async fn connect(
    address: SocketAddr,
    tls: ClientTlsConfig,
) -> Result<Channel, tonic::transport::Error> {
    Endpoint::from_shared(format!("https://{address}"))
        .unwrap()
        .tls_config(tls)?
        .connect()
        .await
}

fn invocation_ref(
    claims: &WorkloadIdentityClaims,
    profile: &RuntimeInvocationContext,
) -> RuntimeInvocationRef {
    RuntimeInvocationRef {
        schema_version: 1,
        tenant_id: claims.tenant_id.to_string(),
        application_id: claims.application_id.to_string(),
        workload_identity_id: claims.workload_identity_id.to_string(),
        workspace_id: profile.workspace_id.to_string(),
        agent_version_id: profile.agent_version_id.to_string(),
        model_policy_id: profile.model_policy_id.to_string(),
    }
}

fn profile_for(claims: &WorkloadIdentityClaims) -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: claims.tenant_id,
        application_id: claims.application_id,
        workload_identity_id: claims.workload_identity_id,
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_run_completes_over_mutual_tls() {
    let signing_key = SigningKey::from_bytes(&[101; 32]);
    let pki = test_pki();
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let provider = spawn_provider().await;
    let claims = operator_claims(Uuid::now_v7());
    let token = sign(&signing_key, &claims);
    let profile = profile_for(&claims);

    let address = spawn_tls_surface(
        &signing_key,
        profile,
        config(&state, &workspace, provider),
        pki.server,
    )
    .await;

    let channel = connect(address, pki.client.into_tonic())
        .await
        .expect("a certificate signed by the server's CA must complete the handshake");
    let mut client = RuntimeInvocationClient::new(channel);
    let invocation = invocation_ref(&claims, &profile);
    let run_id = Uuid::now_v7();

    client
        .submit(with_token(
            SubmitRunRequest {
                invocation: Some(invocation.clone()),
                run_id: run_id.to_string(),
                input: "say something".into(),
            },
            &token,
        ))
        .await
        .expect("submit over mTLS");

    let status = tokio::time::timeout(Duration::from_secs(20), async {
        let mut cursor = 0_u64;
        loop {
            let page = client
                .read_events(with_token(
                    ReadRunEventsRequest {
                        schema_version: 1,
                        invocation: Some(invocation.clone()),
                        run_id: run_id.to_string(),
                        after_sequence: cursor,
                        limit: 64,
                    },
                    &token,
                ))
                .await
                .expect("read over mTLS")
                .into_inner();
            cursor = page.next_after_sequence;
            match page.boundary.and_then(|boundary| boundary.boundary) {
                Some(Boundary::Terminal(terminal)) => return terminal.status,
                Some(Boundary::Retired(retired)) => return retired.status,
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .expect("the Run did not reach a terminal boundary over mTLS");

    assert_eq!(status, "succeeded");
}

/// Without this, the surface would be running TLS, not mutual TLS: any client
/// that trusts the server's CA could reach it.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_presenting_no_certificate_is_refused() {
    let signing_key = SigningKey::from_bytes(&[102; 32]);
    let pki = test_pki();
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let provider = spawn_provider().await;
    let claims = operator_claims(Uuid::now_v7());
    let token = sign(&signing_key, &claims);
    let profile = profile_for(&claims);

    let address = spawn_tls_surface(
        &signing_key,
        profile,
        config(&state, &workspace, provider),
        pki.server,
    )
    .await;

    // Control: the same server, a good certificate. Without this a server that
    // never started would make the refusal below pass for the wrong reason.
    connect(address, pki.control_client.into_tonic())
        .await
        .expect("the surface must be reachable with a trusted certificate");

    // Trusts the server, offers nothing in return.
    let anonymous = ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(pki.ca_pem))
        .domain_name(SERVER_DOMAIN);

    // Rejection may land on the connect or on the first request depending on
    // when the peer closes; either is a refusal, and neither may succeed.
    let refused = match connect(address, anonymous).await {
        Err(_) => true,
        Ok(channel) => RuntimeInvocationClient::new(channel)
            .submit(with_token(
                SubmitRunRequest {
                    invocation: Some(invocation_ref(&claims, &profile)),
                    run_id: Uuid::now_v7().to_string(),
                    input: "say something".into(),
                },
                &token,
            ))
            .await
            .is_err(),
    };

    assert!(
        refused,
        "a client with no certificate reached a surface that requires mutual TLS"
    );
}

/// A correctly-formed certificate from an authority the server does not trust
/// must fail for that reason alone, which is what makes the CA pin real rather
/// than decorative.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_certificate_from_another_authority_is_refused() {
    let signing_key = SigningKey::from_bytes(&[103; 32]);
    let pki = test_pki();
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let provider = spawn_provider().await;
    let claims = operator_claims(Uuid::now_v7());
    let token = sign(&signing_key, &claims);
    let profile = profile_for(&claims);

    let address = spawn_tls_surface(
        &signing_key,
        profile,
        config(&state, &workspace, provider),
        pki.server,
    )
    .await;

    // Control: the same server, a good certificate. Without this a server that
    // never started would make the refusal below pass for the wrong reason.
    connect(address, pki.control_client.into_tonic())
        .await
        .expect("the surface must be reachable with a trusted certificate");

    let refused = match connect(address, pki.foreign_client.into_tonic()).await {
        Err(_) => true,
        Ok(channel) => RuntimeInvocationClient::new(channel)
            .submit(with_token(
                SubmitRunRequest {
                    invocation: Some(invocation_ref(&claims, &profile)),
                    run_id: Uuid::now_v7().to_string(),
                    input: "say something".into(),
                },
                &token,
            ))
            .await
            .is_err(),
    };

    assert!(
        refused,
        "a certificate from an untrusted authority reached the surface"
    );
}
