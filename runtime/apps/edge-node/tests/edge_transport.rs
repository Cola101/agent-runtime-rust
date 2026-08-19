use agent_edge_node::transport::{EdgeOutboundConfig, EdgeOutboundConnector};
use agent_edge_node::wire::edge_node_session_server::{EdgeNodeSession, EdgeNodeSessionServer};
use agent_edge_node::wire::{
    ControlToNode, EnrollmentRevoked, NodeToControl, OutboxAck, SessionAccepted, SessionChallenge,
    TaskDelivery, control_to_node, node_to_control,
};
use agent_edge_node::{
    EdgeControlPlaneTrust, EdgeDeviceIdentity, EdgeEnrollmentRevocationClaims, EdgeNode,
    EdgeNodeError, EdgeNodeStore, EdgeOutboxAckClaims, EdgeOutboxRecord, VerifiedEdgeEnrollment,
    verify_edge_session_proof,
};
use agent_grpc_security::{ClientMtlsMaterials, ServerMtlsMaterials};
use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{
    EDGE_TASK_SCHEMA_VERSION, EdgeTaskClaims, RUNTIME_INVOCATION_SCHEMA_VERSION, RunBudget,
    RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer as _, SigningKey};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_stream::Stream;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use uuid::Uuid;

mod common;

const CONTROL_KEY_ID: &str = "transport-control-2026-08";

fn invocation() -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: RUNTIME_INVOCATION_SCHEMA_VERSION,
        tenant_id: Uuid::from_u128(3001),
        application_id: Uuid::from_u128(3002),
        workload_identity_id: Uuid::from_u128(3003),
        workspace_id: Uuid::from_u128(3004),
        agent_version_id: Uuid::from_u128(3005),
        model_policy_id: Uuid::from_u128(3006),
    }
}

fn runtime(runtime_state: PathBuf, workspace_root: PathBuf, endpoint: String) -> EmbeddedRuntime {
    let config = LocalRuntimeConfig {
        state_root: runtime_state,
        workspace_root,
        agent_instructions: "Return only the edge transport result.".into(),
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
                endpoint,
                model: "edge-transport-test".into(),
                api_key: "test-key".into(),
                region: "local".into(),
                accepted_data_classes: BTreeSet::from([DataClass::Internal]),
                capabilities: BTreeSet::from([Capability::Text]),
                healthy: true,
                latency_ms: 1,
                cost_per_million_tokens_micros: 1,
                response_timeout_ms: 5_000,
                stream_idle_timeout_ms: 5_000,
                max_output_tokens: None,
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
    };
    EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 2,
            max_active_runs_per_tenant: 2,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 4,
            max_queued_runs_per_tenant: 4,
        },
        vec![RuntimeProfile {
            invocation: invocation(),
            config,
        }],
    )
    .expect("embedded Runtime")
}

fn signed_task(key: &SigningKey, enrollment: &VerifiedEdgeEnrollment, now: i64) -> String {
    let run_id = Uuid::from_u128(3010);
    let claims = EdgeTaskClaims {
        schema_version: EDGE_TASK_SCHEMA_VERSION,
        task_id: Uuid::from_u128(3007),
        enrollment_id: enrollment.claims().enrollment_id,
        node_id: enrollment.claims().node_id,
        node_generation: enrollment.claims().node_generation,
        capability_manifest_digest: enrollment.claims().capability_manifest_digest.clone(),
        required_capabilities: BTreeSet::from(["runtime.agent.execute".into()]),
        issued_at_unix_ms: now - 1_000,
        expires_at_unix_ms: now + 60_000,
        invocation: invocation(),
        run_id,
        session_id: run_id,
        workspace_owner_epoch: 31,
        input: "prove the authenticated edge transport".into(),
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("task claims"));
    let signed = format!("edge-task-v1.{CONTROL_KEY_ID}.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes()).to_bytes());
    format!("{signed}.{signature}")
}

async fn spawn_provider() -> (String, tokio::task::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
    );
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("provider request");
        let mut request = vec![0_u8; 64 * 1024];
        let _ = socket.read(&mut request).await;
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"mtls-edge-ok\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.expect("reply");
        1
    });
    (endpoint, task)
}

struct TestControlPlane {
    enrollment: VerifiedEdgeEnrollment,
    task_token: String,
    signing_key: SigningKey,
    uploaded: Arc<Mutex<Option<Vec<EdgeOutboxRecord>>>>,
    drop_first_upload: bool,
    sessions: Arc<AtomicUsize>,
    completed: Arc<Notify>,
    revoke_after_accept: bool,
}

#[tonic::async_trait]
impl EdgeNodeSession for TestControlPlane {
    type OpenSessionStream =
        Pin<Box<dyn Stream<Item = Result<ControlToNode, Status>> + Send + 'static>>;

    async fn open_session(
        &self,
        request: Request<tonic::Streaming<NodeToControl>>,
    ) -> Result<Response<Self::OpenSessionStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(8);
        let enrollment = self.enrollment.clone();
        let task_token = self.task_token.clone();
        let signing_key = self.signing_key.clone();
        let uploaded = self.uploaded.clone();
        let session_index = self.sessions.fetch_add(1, Ordering::SeqCst);
        let drop_first_upload = self.drop_first_upload;
        let completed = self.completed.clone();
        let revoke_after_accept = self.revoke_after_accept;
        tokio::spawn(async move {
            let hello = inbound
                .message()
                .await?
                .ok_or_else(|| Status::invalid_argument("hello"))?;
            let Some(node_to_control::Frame::Hello(hello)) = hello.frame else {
                return Err(Status::invalid_argument("first frame must be hello"));
            };
            if hello.enrollment_id != enrollment.claims().enrollment_id.to_string()
                || hello.node_generation != enrollment.claims().node_generation
            {
                return Err(Status::permission_denied("wrong enrollment"));
            }
            let session_id = Uuid::from_u128(3020);
            let nonce = [32_u8; 32];
            tx.send(Ok(ControlToNode {
                frame: Some(control_to_node::Frame::Challenge(SessionChallenge {
                    schema_version: 1,
                    session_id: session_id.to_string(),
                    nonce: nonce.to_vec(),
                    expires_at_unix_ms: chrono::Utc::now().timestamp_millis() + 60_000,
                })),
            }))
            .await
            .map_err(|_| Status::cancelled("client closed"))?;
            let proof = inbound
                .message()
                .await?
                .ok_or_else(|| Status::invalid_argument("proof"))?;
            let Some(node_to_control::Frame::Proof(proof)) = proof.frame else {
                return Err(Status::invalid_argument("second frame must be proof"));
            };
            verify_edge_session_proof(
                &proof.proof_token,
                &enrollment,
                session_id,
                &nonce,
                chrono::Utc::now().timestamp_millis(),
            )
            .map_err(|_| Status::permission_denied("invalid device proof"))?;
            tx.send(Ok(ControlToNode {
                frame: Some(control_to_node::Frame::Accepted(SessionAccepted {
                    schema_version: 1,
                    session_id: session_id.to_string(),
                })),
            }))
            .await
            .map_err(|_| Status::cancelled("client closed"))?;
            if revoke_after_accept {
                let now = chrono::Utc::now().timestamp_millis();
                let claims = EdgeEnrollmentRevocationClaims {
                    schema_version: 1,
                    revocation_id: Uuid::from_u128(3022),
                    enrollment_id: enrollment.claims().enrollment_id,
                    device_id: enrollment.claims().device_id,
                    node_id: enrollment.claims().node_id,
                    node_generation: enrollment.claims().node_generation,
                    reason_code: "operator_revoked".into(),
                    issued_at_unix_ms: now - 1_000,
                    expires_at_unix_ms: now + 60_000,
                };
                let payload =
                    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("revocation claims"));
                let signed = format!("edge-enrollment-revocation-v1.{CONTROL_KEY_ID}.{payload}");
                let signature =
                    URL_SAFE_NO_PAD.encode(signing_key.sign(signed.as_bytes()).to_bytes());
                tx.send(Ok(ControlToNode {
                    frame: Some(control_to_node::Frame::Revoked(EnrollmentRevoked {
                        schema_version: 1,
                        revocation_token: format!("{signed}.{signature}"),
                    })),
                }))
                .await
                .map_err(|_| Status::cancelled("client closed"))?;
                completed.notify_one();
                return Ok::<(), Status>(());
            }
            tx.send(Ok(ControlToNode {
                frame: Some(control_to_node::Frame::Task(TaskDelivery {
                    schema_version: 1,
                    task_token,
                })),
            }))
            .await
            .map_err(|_| Status::cancelled("client closed"))?;
            let upload = inbound
                .message()
                .await?
                .ok_or_else(|| Status::invalid_argument("upload"))?;
            let Some(node_to_control::Frame::OutboxBatch(upload)) = upload.frame else {
                return Err(Status::invalid_argument("expected outbox batch"));
            };
            let records = serde_json::from_slice::<Vec<EdgeOutboxRecord>>(&upload.records_json)
                .map_err(|_| Status::invalid_argument("records JSON"))?;
            if upload.batch_digest != hex::encode(Sha256::digest(&upload.records_json))
                || records.first().map(|record| record.sequence) != Some(upload.first_sequence)
                || records.last().map(|record| record.sequence) != Some(upload.last_sequence)
            {
                return Err(Status::invalid_argument("invalid batch evidence"));
            }
            *uploaded.lock().await = Some(records);
            if drop_first_upload && session_index == 0 {
                return Ok::<(), Status>(());
            }
            let now = chrono::Utc::now().timestamp_millis();
            let claims = EdgeOutboxAckClaims {
                schema_version: 1,
                ack_id: Uuid::from_u128(3021),
                session_id,
                enrollment_id: enrollment.claims().enrollment_id,
                node_id: enrollment.claims().node_id,
                node_generation: enrollment.claims().node_generation,
                through_sequence: upload.last_sequence,
                batch_digest: upload.batch_digest,
                issued_at_unix_ms: now - 1_000,
                expires_at_unix_ms: now + 60_000,
            };
            let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("ACK claims"));
            let signed = format!("edge-outbox-ack-v1.{CONTROL_KEY_ID}.{payload}");
            let signature = URL_SAFE_NO_PAD.encode(signing_key.sign(signed.as_bytes()).to_bytes());
            tx.send(Ok(ControlToNode {
                frame: Some(control_to_node::Frame::Ack(OutboxAck {
                    schema_version: 1,
                    ack_token: format!("{signed}.{signature}"),
                })),
            }))
            .await
            .map_err(|_| Status::cancelled("client closed"))?;
            completed.notify_one();
            Ok::<(), Status>(())
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// The production break this catches is claiming a connected Edge Node while
/// only testing serialization or plaintext loopback. The real node must prove
/// its durable device key over mutual TLS, execute a signed task, upload exact
/// durable records and retain them until a batch-bound signed ACK arrives.
#[tokio::test]
async fn mutual_tls_session_proves_device_executes_task_and_prunes_only_signed_ack() {
    let edge_state = tempfile::tempdir().expect("edge state");
    let runtime_state = tempfile::tempdir().expect("runtime state");
    let workspace = tempfile::tempdir().expect("workspace");
    let now = chrono::Utc::now().timestamp_millis();
    let identity = EdgeDeviceIdentity::load_or_create(edge_state.path()).expect("identity");
    let enrollment = common::verified_enrollment(
        edge_state.path(),
        Uuid::from_u128(3030),
        Uuid::from_u128(3031),
        7,
        now,
    );
    let control_key = SigningKey::from_bytes(&[93; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(
        CONTROL_KEY_ID.into(),
        control_key.verifying_key(),
    )]))
    .expect("trust");
    let task_token = signed_task(&control_key, &enrollment, now);
    let (provider_endpoint, provider) = spawn_provider().await;
    let node = Arc::new(
        EdgeNode::new(
            enrollment.clone(),
            trust,
            EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).expect("store"),
            runtime(
                runtime_state.path().to_path_buf(),
                workspace.path().canonicalize().expect("workspace"),
                provider_endpoint,
            ),
        )
        .expect("node"),
    );
    let uploaded = Arc::new(Mutex::new(None));
    let completed = Arc::new(Notify::new());
    let service = TestControlPlane {
        enrollment,
        task_token,
        signing_key: control_key,
        uploaded: uploaded.clone(),
        drop_first_upload: false,
        sessions: Arc::new(AtomicUsize::new(0)),
        completed: completed.clone(),
        revoke_after_accept: false,
    };
    let (server_tls, client_tls) = test_pki("edge-control.test");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("control bind");
    let address = listener.local_addr().expect("control address");
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .tls_config(server_tls.into_tonic())
            .expect("server TLS")
            .add_service(EdgeNodeSessionServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .expect("control server");
    });
    let connector = EdgeOutboundConnector::new(
        identity,
        node.clone(),
        EdgeOutboundConfig::new(format!("https://{address}"), client_tls).expect("outbound config"),
    );

    connector.connect_once().await.expect("mTLS session");

    assert_eq!(provider.await.expect("provider task"), 1);
    let uploaded = uploaded.lock().await;
    let records = uploaded.as_ref().expect("uploaded records");
    assert!(records.len() >= 4);
    assert!(node.pending_outbox(0, 256).expect("outbox").is_empty());
    shutdown.send(()).ok();
    server.await.expect("server task");
}

/// The production break this catches is treating a lost ACK as permission to
/// forget uploaded records or re-execute the task. The next mutually
/// authenticated session must resend the durable batch, converge the duplicate
/// task from its receipt, and call the model exactly once.
#[tokio::test]
async fn reconnect_resends_unacked_batch_without_reexecuting_the_task() {
    let edge_state = tempfile::tempdir().expect("edge state");
    let runtime_state = tempfile::tempdir().expect("runtime state");
    let workspace = tempfile::tempdir().expect("workspace");
    let now = chrono::Utc::now().timestamp_millis();
    let identity = EdgeDeviceIdentity::load_or_create(edge_state.path()).expect("identity");
    let enrollment = common::verified_enrollment(
        edge_state.path(),
        Uuid::from_u128(3040),
        Uuid::from_u128(3041),
        8,
        now,
    );
    let control_key = SigningKey::from_bytes(&[94; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(
        CONTROL_KEY_ID.into(),
        control_key.verifying_key(),
    )]))
    .expect("trust");
    let task_token = signed_task(&control_key, &enrollment, now);
    let (provider_endpoint, provider) = spawn_provider().await;
    let node = Arc::new(
        EdgeNode::new(
            enrollment.clone(),
            trust,
            EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).expect("store"),
            runtime(
                runtime_state.path().to_path_buf(),
                workspace.path().canonicalize().expect("workspace"),
                provider_endpoint,
            ),
        )
        .expect("node"),
    );
    let sessions = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(Notify::new());
    let service = TestControlPlane {
        enrollment,
        task_token,
        signing_key: control_key,
        uploaded: Arc::new(Mutex::new(None)),
        drop_first_upload: true,
        sessions: sessions.clone(),
        completed: completed.clone(),
        revoke_after_accept: false,
    };
    let (server_tls, client_tls) = test_pki("edge-control.test");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("control bind");
    let address = listener.local_addr().expect("control address");
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .tls_config(server_tls.into_tonic())
            .expect("server TLS")
            .add_service(EdgeNodeSessionServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .expect("control server");
    });
    let connector = EdgeOutboundConnector::new(
        identity,
        node.clone(),
        EdgeOutboundConfig::new(format!("https://{address}"), client_tls)
            .expect("outbound config")
            .with_reconnect_delays(Duration::from_millis(10), Duration::from_millis(20))
            .expect("reconnect policy"),
    );
    let stop = CancellationToken::new();
    let runner_stop = stop.clone();
    let runner = tokio::spawn(async move { connector.run(runner_stop).await });

    tokio::time::timeout(Duration::from_secs(10), completed.notified())
        .await
        .expect("second session ACK");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if node.pending_outbox(0, 256).expect("outbox").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("client applied the signed ACK");
    stop.cancel();
    runner.await.expect("connector task").expect("connector");

    assert_eq!(provider.await.expect("provider task"), 1);
    assert_eq!(sessions.load(Ordering::SeqCst), 2);
    assert!(node.pending_outbox(0, 256).expect("outbox").is_empty());
    shutdown.send(()).ok();
    server.await.expect("server task");
}

/// The production break this catches is logging an online revocation but
/// continuing to reconnect or accept tasks. The signed control-plane decision
/// must become durable local state and terminate the current connector.
#[tokio::test]
async fn signed_online_revocation_terminates_session_and_survives_restart() {
    let edge_state = tempfile::tempdir().expect("edge state");
    let runtime_state = tempfile::tempdir().expect("runtime state");
    let workspace = tempfile::tempdir().expect("workspace");
    let now = chrono::Utc::now().timestamp_millis();
    let identity = EdgeDeviceIdentity::load_or_create(edge_state.path()).expect("identity");
    let enrollment = common::verified_enrollment(
        edge_state.path(),
        Uuid::from_u128(3050),
        Uuid::from_u128(3051),
        9,
        now,
    );
    let control_key = SigningKey::from_bytes(&[95; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(
        CONTROL_KEY_ID.into(),
        control_key.verifying_key(),
    )]))
    .expect("trust");
    let node = Arc::new(
        EdgeNode::new(
            enrollment.clone(),
            trust,
            EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).expect("store"),
            runtime(
                runtime_state.path().to_path_buf(),
                workspace.path().canonicalize().expect("workspace"),
                "http://127.0.0.1:1/v1/chat/completions".into(),
            ),
        )
        .expect("node"),
    );
    let completed = Arc::new(Notify::new());
    let service = TestControlPlane {
        enrollment: enrollment.clone(),
        task_token: "must-not-be-delivered".into(),
        signing_key: control_key,
        uploaded: Arc::new(Mutex::new(None)),
        drop_first_upload: false,
        sessions: Arc::new(AtomicUsize::new(0)),
        completed,
        revoke_after_accept: true,
    };
    let (server_tls, client_tls) = test_pki("edge-control.test");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("control bind");
    let address = listener.local_addr().expect("control address");
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .tls_config(server_tls.into_tonic())
            .expect("server TLS")
            .add_service(EdgeNodeSessionServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .expect("control server");
    });
    let connector = EdgeOutboundConnector::new(
        identity,
        node.clone(),
        EdgeOutboundConfig::new(format!("https://{address}"), client_tls).expect("config"),
    );

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        connector.run(CancellationToken::new()),
    )
    .await
    .expect("revocation must stop reconnecting")
    .expect_err("revocation is terminal");
    assert_eq!(error, EdgeNodeError::EnrollmentRevoked);
    drop(connector);
    drop(node);
    assert!(EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).is_err());
    shutdown.send(()).ok();
    server.await.expect("server task");
}

fn test_pki(domain_name: &str) -> (ServerMtlsMaterials, ClientMtlsMaterials) {
    let mut ca_params = CertificateParams::new(Vec::new()).expect("CA params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_key = KeyPair::generate().expect("CA key");
    let ca_cert = ca_params.self_signed(&ca_key).expect("CA certificate");
    let issuer = Issuer::new(ca_params, ca_key);
    let server_key = KeyPair::generate().expect("server key");
    let mut server_params =
        CertificateParams::new(vec![domain_name.into()]).expect("server params");
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params
        .signed_by(&server_key, &issuer)
        .expect("server certificate");
    let client_key = KeyPair::generate().expect("client key");
    let mut client_params =
        CertificateParams::new(vec!["edge-node.test".into()]).expect("client params");
    client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params
        .signed_by(&client_key, &issuer)
        .expect("client certificate");
    let ca_pem = ca_cert.pem().into_bytes();
    (
        ServerMtlsMaterials::new(
            server_cert.pem().into_bytes(),
            server_key.serialize_pem().into_bytes(),
            ca_pem.clone(),
        )
        .expect("server TLS materials"),
        ClientMtlsMaterials::new(
            client_cert.pem().into_bytes(),
            client_key.serialize_pem().into_bytes(),
            ca_pem,
            domain_name.into(),
        )
        .expect("client TLS materials"),
    )
}
