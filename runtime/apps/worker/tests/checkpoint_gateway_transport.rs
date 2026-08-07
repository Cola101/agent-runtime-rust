use agent_checkpoint_gateway::{CheckpointStorageGrpcService, checkpoint_storage_server};
use agent_grpc_security::{ClientMtlsMaterials, ServerMtlsMaterials};
use agent_runtime_worker::{
    CheckpointPayloadStore, CheckpointStoreContext, GrpcCheckpointPayloadStore,
};
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
use tonic::transport::Server;
use uuid::Uuid;

#[tokio::test]
async fn worker_store_client_uses_its_bound_token_for_put_and_get() {
    let signing_key = SigningKey::from_bytes(&[12; 32]);
    let service = CheckpointStorageGrpcService::new(
        Arc::new(InMemory::new()),
        WorkloadTokenVerifier::new(signing_key.verifying_key()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (server_tls, client_tls) = test_pki();
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
    let claims = claims();
    let context = CheckpointStoreContext {
        tenant_id: claims.tenant_id,
        run_id: claims.run_id,
        attempt_id: claims.attempt_id,
        worker_id: claims.worker_id,
        worker_incarnation_id: claims.worker_incarnation_id,
        workload_token: sign(&signing_key, &claims),
    };
    let store =
        GrpcCheckpointPayloadStore::connect_with_mtls(format!("https://{address}"), client_tls)
            .await
            .unwrap();
    let payload = b"worker checkpoint client bytes".to_vec();
    let digest = hex::encode(Sha256::digest(&payload));
    let payload_ref = format!("checkpoint://sha256/{digest}");

    store.put(&context, &payload_ref, &payload).await.unwrap();
    let restored = store.get(&context, &payload_ref).await.unwrap();

    assert_eq!(restored, payload);
    shutdown.send(()).ok();
    server.await.unwrap();
}

fn test_pki() -> (ServerMtlsMaterials, ClientMtlsMaterials) {
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
    let mut server_params = CertificateParams::new(vec!["checkpoint-gateway.test".into()]).unwrap();
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();
    let client_key = KeyPair::generate().unwrap();
    let mut client_params = CertificateParams::new(vec!["runtime-worker.test".into()]).unwrap();
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();
    let ca_pem = ca_cert.pem().into_bytes();
    (
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
            "checkpoint-gateway.test".into(),
        )
        .unwrap(),
    )
}

fn claims() -> WorkloadIdentityClaims {
    let now = chrono::Utc::now().timestamp_millis();
    WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
        model_policy_digest: String::new(),
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
