use agent_checkpoint_gateway::{
    CheckpointStorageGrpcService, MAX_STORED_CHECKPOINT_BYTES, checkpoint_storage_server,
};
use agent_checkpoint_gateway_protocol::v1::checkpoint_storage_client::CheckpointStorageClient;
use agent_checkpoint_gateway_protocol::v1::{
    GetCheckpointRequest, PutCheckpointRequest, WorkloadBinding,
};
use agent_grpc_security::{ClientMtlsMaterials, ServerMtlsMaterials};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use object_store::memory::InMemory;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Server};
use tonic::{Code, Request};
use uuid::Uuid;

#[tokio::test]
async fn authorized_worker_round_trips_a_content_addressed_checkpoint() {
    let fixture = GatewayFixture::start().await;
    let claims = claims();
    let payload = b"compressed checkpoint bytes".to_vec();
    let digest = hex::encode(Sha256::digest(&payload));
    let payload_ref = format!("checkpoint://sha256/{digest}");

    let put = fixture
        .put(&claims, &payload_ref, payload.clone())
        .await
        .unwrap();
    let get = fixture.get(&claims, &payload_ref).await.unwrap();

    assert_eq!(put.stored_payload_digest, digest);
    assert_eq!(put.stored_size, payload.len() as u64);
    assert_eq!(get.payload, payload);
    fixture.stop().await;
}

#[tokio::test]
async fn token_cannot_cross_tenant_or_worker_incarnation_boundaries() {
    let fixture = GatewayFixture::start().await;
    let claims = claims();
    let payload = b"tenant private checkpoint".to_vec();
    let digest = hex::encode(Sha256::digest(&payload));
    let payload_ref = format!("checkpoint://sha256/{digest}");
    let mut wrong_tenant = binding(&claims);
    wrong_tenant.tenant_id = Uuid::now_v7().to_string();
    let mut wrong_incarnation = binding(&claims);
    wrong_incarnation.worker_incarnation_id = Uuid::now_v7().to_string();

    let tenant_error = fixture
        .put_with_binding(&claims, wrong_tenant, &payload_ref, payload.clone())
        .await
        .unwrap_err();
    let incarnation_error = fixture
        .put_with_binding(&claims, wrong_incarnation, &payload_ref, payload)
        .await
        .unwrap_err();

    assert_eq!(tenant_error.code(), Code::PermissionDenied);
    assert_eq!(incarnation_error.code(), Code::PermissionDenied);
    fixture.stop().await;
}

/// The production break this catches is authorizing a v20 checkpoint through
/// only tenant/Run/Worker fields. A token for one Workspace must not store into
/// another Workspace even when every legacy field is unchanged.
#[tokio::test]
async fn v2_checkpoint_binding_rejects_a_different_workspace() {
    let fixture = GatewayFixture::start().await;
    let claims = claims_v4();
    let payload = b"workspace-bound checkpoint".to_vec();
    let digest = hex::encode(Sha256::digest(&payload));
    let payload_ref = format!("checkpoint://sha256/{digest}");
    let mut wrong = binding(&claims);
    wrong.workspace_id = Uuid::now_v7().to_string();
    let request = PutCheckpointRequest {
        schema_version: 2,
        binding: Some(wrong),
        payload_ref,
        payload,
    };
    let mut request = Request::new(request);
    authorize(&mut request, &sign(&fixture.signing_key, &claims));
    let mut client = CheckpointStorageClient::connect(fixture.endpoint.clone())
        .await
        .unwrap();

    let error = client.put_checkpoint(request).await.unwrap_err();

    assert_eq!(error.code(), Code::PermissionDenied);
    fixture.stop().await;
}

#[tokio::test]
async fn missing_checkpoint_is_reported_as_not_found() {
    let fixture = GatewayFixture::start().await;
    let claims = claims();
    let payload_ref = format!("checkpoint://sha256/{}", "a".repeat(64));

    let error = fixture.get(&claims, &payload_ref).await.unwrap_err();

    assert_eq!(error.code(), Code::NotFound);
    fixture.stop().await;
}

#[tokio::test]
async fn oversized_or_digest_mismatched_checkpoint_is_rejected_before_storage() {
    let fixture = GatewayFixture::start().await;
    let claims = claims();
    let payload_ref = format!("checkpoint://sha256/{}", "a".repeat(64));

    let digest_error = fixture
        .put(&claims, &payload_ref, b"different bytes".to_vec())
        .await
        .unwrap_err();
    let size_error = fixture
        .put(
            &claims,
            &payload_ref,
            vec![0; MAX_STORED_CHECKPOINT_BYTES + 1],
        )
        .await
        .unwrap_err();

    assert_eq!(digest_error.code(), Code::InvalidArgument);
    assert_eq!(size_error.code(), Code::InvalidArgument);
    fixture.stop().await;
}

#[tokio::test]
async fn checkpoint_gateway_requires_a_client_certificate_signed_by_its_trusted_ca() {
    let fixture = MtlsGatewayFixture::start().await;
    let claims = claims();
    let payload = b"mutually authenticated checkpoint".to_vec();
    let digest = hex::encode(Sha256::digest(&payload));
    let payload_ref = format!("checkpoint://sha256/{digest}");

    let stored = fixture.put(&claims, &payload_ref, payload).await.unwrap();
    let unauthenticated = Endpoint::from_shared(fixture.endpoint.clone())
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(fixture.ca_pem.clone()))
                .domain_name("checkpoint-gateway.test"),
        )
        .unwrap()
        .connect()
        .await;
    let untrusted_ca = test_pki("other.test").0;
    let wrong_ca = Endpoint::from_shared(fixture.endpoint.clone())
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(untrusted_ca))
                .domain_name("checkpoint-gateway.test"),
        )
        .unwrap()
        .connect()
        .await;

    let unauthenticated_rejected = match unauthenticated {
        Err(_) => true,
        Ok(channel) => {
            let probe = b"missing client identity".to_vec();
            let probe_digest = hex::encode(Sha256::digest(&probe));
            let mut request = Request::new(PutCheckpointRequest {
                schema_version: 1,
                binding: Some(binding(&claims)),
                payload_ref: format!("checkpoint://sha256/{probe_digest}"),
                payload: probe,
            });
            authorize(&mut request, &sign(&fixture.signing_key, &claims));
            CheckpointStorageClient::new(channel)
                .put_checkpoint(request)
                .await
                .is_err()
        }
    };

    assert_eq!(stored.stored_payload_digest, digest);
    assert!(unauthenticated_rejected);
    assert!(wrong_ca.is_err());
    fixture.stop().await;
}

struct MtlsGatewayFixture {
    endpoint: String,
    ca_pem: Vec<u8>,
    client_tls: ClientMtlsMaterials,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    signing_key: SigningKey,
}

impl MtlsGatewayFixture {
    async fn start() -> Self {
        let signing_key = SigningKey::from_bytes(&[43; 32]);
        let service = CheckpointStorageGrpcService::new(
            Arc::new(InMemory::new()),
            WorkloadTokenVerifier::new(signing_key.verifying_key()),
        );
        let (ca_pem, server_tls, client_tls) = test_pki("checkpoint-gateway.test");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            Server::builder()
                .tls_config(server_tls.into_tonic())
                .unwrap()
                .add_service(checkpoint_storage_server(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    shutdown_rx.await.ok();
                })
                .await
                .unwrap();
        });
        Self {
            endpoint: format!("https://{address}"),
            ca_pem,
            client_tls,
            shutdown,
            server,
            signing_key,
        }
    }

    async fn put(
        &self,
        claims: &WorkloadIdentityClaims,
        payload_ref: &str,
        payload: Vec<u8>,
    ) -> Result<agent_checkpoint_gateway_protocol::v1::PutCheckpointResponse, tonic::Status> {
        let channel = Endpoint::from_shared(self.endpoint.clone())
            .unwrap()
            .tls_config(self.client_tls.clone().into_tonic())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = CheckpointStorageClient::new(channel);
        let mut request = Request::new(PutCheckpointRequest {
            schema_version: 1,
            binding: Some(binding(claims)),
            payload_ref: payload_ref.to_string(),
            payload,
        });
        authorize(&mut request, &sign(&self.signing_key, claims));
        Ok(client.put_checkpoint(request).await?.into_inner())
    }

    async fn stop(self) {
        self.shutdown.send(()).ok();
        self.server.await.unwrap();
    }
}

fn test_pki(domain_name: &str) -> (Vec<u8>, ServerMtlsMaterials, ClientMtlsMaterials) {
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().unwrap();
    let mut server_params = CertificateParams::new(vec![domain_name.into()]).unwrap();
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

    let client_key = KeyPair::generate().unwrap();
    let mut client_params = CertificateParams::new(vec!["runtime-worker.test".into()]).unwrap();
    client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();
    let ca_pem = ca_cert.pem().into_bytes();
    (
        ca_pem.clone(),
        ServerMtlsMaterials::new(
            server_cert.pem().into_bytes(),
            server_key.serialize_pem().into_bytes(),
            ca_pem.clone(),
        )
        .unwrap(),
        ClientMtlsMaterials::new(
            client_cert.pem().into_bytes(),
            client_key.serialize_pem().into_bytes(),
            ca_pem,
            domain_name.into(),
        )
        .unwrap(),
    )
}

struct GatewayFixture {
    endpoint: String,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    signing_key: SigningKey,
}

impl GatewayFixture {
    async fn start() -> Self {
        let signing_key = SigningKey::from_bytes(&[11; 32]);
        let service = CheckpointStorageGrpcService::new(
            Arc::new(InMemory::new()),
            WorkloadTokenVerifier::new(signing_key.verifying_key()),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(checkpoint_storage_server(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    shutdown_rx.await.ok();
                })
                .await
                .unwrap();
        });
        Self {
            endpoint: format!("http://{address}"),
            shutdown,
            server,
            signing_key,
        }
    }

    async fn put(
        &self,
        claims: &WorkloadIdentityClaims,
        payload_ref: &str,
        payload: Vec<u8>,
    ) -> Result<agent_checkpoint_gateway_protocol::v1::PutCheckpointResponse, tonic::Status> {
        self.put_with_binding(claims, binding(claims), payload_ref, payload)
            .await
    }

    async fn put_with_binding(
        &self,
        claims: &WorkloadIdentityClaims,
        binding: WorkloadBinding,
        payload_ref: &str,
        payload: Vec<u8>,
    ) -> Result<agent_checkpoint_gateway_protocol::v1::PutCheckpointResponse, tonic::Status> {
        let request = PutCheckpointRequest {
            schema_version: 1,
            binding: Some(binding),
            payload_ref: payload_ref.to_string(),
            payload,
        };
        let mut request = Request::new(request);
        authorize(&mut request, &sign(&self.signing_key, claims));
        let mut client = CheckpointStorageClient::connect(self.endpoint.clone())
            .await
            .unwrap();
        Ok(client.put_checkpoint(request).await?.into_inner())
    }

    async fn get(
        &self,
        claims: &WorkloadIdentityClaims,
        payload_ref: &str,
    ) -> Result<agent_checkpoint_gateway_protocol::v1::GetCheckpointResponse, tonic::Status> {
        let request = GetCheckpointRequest {
            schema_version: 1,
            binding: Some(binding(claims)),
            payload_ref: payload_ref.to_string(),
        };
        let mut request = Request::new(request);
        authorize(&mut request, &sign(&self.signing_key, claims));
        let mut client = CheckpointStorageClient::connect(self.endpoint.clone())
            .await
            .unwrap();
        Ok(client.get_checkpoint(request).await?.into_inner())
    }

    async fn stop(self) {
        self.shutdown.send(()).ok();
        self.server.await.unwrap();
    }
}

fn authorize<T>(request: &mut Request<T>, token: &str) {
    request.metadata_mut().insert(
        "authorization",
        MetadataValue::try_from(format!("Bearer {token}")).unwrap(),
    );
}

fn binding(claims: &WorkloadIdentityClaims) -> WorkloadBinding {
    WorkloadBinding {
        tenant_id: claims.tenant_id.to_string(),
        application_id: claims.application_id.to_string(),
        workload_identity_id: claims.workload_identity_id.to_string(),
        run_id: claims.run_id.to_string(),
        session_id: claims.session_id.to_string(),
        workspace_id: claims.workspace_id.to_string(),
        agent_version_id: claims.agent_version_id.to_string(),
        attempt_id: claims.attempt_id.to_string(),
        worker_id: claims.worker_id.to_string(),
        worker_incarnation_id: claims.worker_incarnation_id.to_string(),
    }
}

fn claims_v4() -> WorkloadIdentityClaims {
    let mut claims = claims();
    claims.schema_version = 4;
    claims.application_id = Uuid::now_v7();
    claims.workload_identity_id = Uuid::now_v7();
    claims.session_id = Uuid::now_v7();
    claims.workspace_id = Uuid::now_v7();
    claims.agent_version_id = Uuid::now_v7();
    claims.model_policy_digest = "a".repeat(64);
    claims
}

fn claims() -> WorkloadIdentityClaims {
    let now = chrono::Utc::now().timestamp_millis();
    WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id: Uuid::now_v7(),
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
        audiences: BTreeSet::from(["checkpoint-gateway".into()]),
        scopes: BTreeSet::from(["checkpoint.read".into(), "checkpoint.write".into()]),
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + 60_000,
    }
}

fn sign(signing_key: &SigningKey, claims: &WorkloadIdentityClaims) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let signature = signing_key.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}
