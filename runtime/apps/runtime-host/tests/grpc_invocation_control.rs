//! Driving a Run over the network, not just being refused by it.
//!
//! Every control assertion so far has been a refusal: no token, wrong shape,
//! wrong tenant, unknown action. Those prove the door is shut. This file proves
//! a command actually reaches a live Run and changes it.
//!
//! It also settles two claims the contract makes and nothing had yet checked.
//! `runtime.proto` says `command_id` is a durable idempotency key, that
//! retrying cannot start a second action, and that reusing the id for a
//! different action is refused by the receipt's command digest. Those sentences
//! were written by me; until now nothing held them to it.

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
    ControlRunRequest, ReadRunEventsRequest, RuntimeInvocationRef, SubmitRunRequest,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::{Channel, Server};
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";

/// Accepts the request and never answers.
///
/// The Run therefore stays live until something cancels it, which is what makes
/// a cancellation observable rather than a race against a Run finishing on its
/// own. The socket is held by the spawned task for the test's lifetime.
async fn spawn_stalling_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            held.push(socket);
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

struct Surface {
    client: RuntimeInvocationClient<Channel>,
    invocation: RuntimeInvocationRef,
    token: String,
    _state: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

async fn spawn_surface(seed: u8) -> Surface {
    let signing_key = SigningKey::from_bytes(&[seed; 32]);
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let provider_endpoint = spawn_stalling_provider().await;
    let claims = operator_claims(Uuid::now_v7());
    let token = sign(&signing_key, &claims);
    let profile = RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: claims.tenant_id,
        application_id: claims.application_id,
        workload_identity_id: claims.workload_identity_id,
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    };
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
            config: LocalRuntimeConfig {
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
                        // Long enough that the Run is cancelled by the command
                        // under test rather than by its own timeout.
                        response_timeout_ms: 120_000,
                        stream_idle_timeout_ms: 120_000,
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
                    max_duration_seconds: 600,
                },
                runtime_policy: RuntimeExecutionPolicySnapshot::default(),
            },
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
            .add_service(RuntimeInvocationServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .ok();
    });
    Surface {
        client: RuntimeInvocationClient::connect(format!("http://{address}"))
            .await
            .unwrap(),
        invocation: RuntimeInvocationRef {
            schema_version: 1,
            tenant_id: claims.tenant_id.to_string(),
            application_id: claims.application_id.to_string(),
            workload_identity_id: claims.workload_identity_id.to_string(),
            workspace_id: profile.workspace_id.to_string(),
            agent_version_id: profile.agent_version_id.to_string(),
            model_policy_id: profile.model_policy_id.to_string(),
        },
        token,
        _state: state,
        _workspace: workspace,
    }
}

impl Surface {
    async fn submit(&mut self, run_id: Uuid) -> u64 {
        self.client
            .submit(with_token(
                SubmitRunRequest {
                    invocation: Some(self.invocation.clone()),
                    run_id: run_id.to_string(),
                    input: "wait for me".into(),
                },
                &self.token,
            ))
            .await
            .expect("submit")
            .into_inner()
            .owner_epoch
    }

    fn cancel(&self, run_id: Uuid, command_id: Uuid, epoch: u64) -> ControlRunRequest {
        ControlRunRequest {
            schema_version: 1,
            invocation: Some(self.invocation.clone()),
            command_id: command_id.to_string(),
            run_id: run_id.to_string(),
            expected_owner_epoch: epoch,
            action_json: br#"{"type":"cancel","reason":"operator stopped it"}"#.to_vec(),
        }
    }

    /// Pages forward until the Run reaches a terminal boundary, returning its
    /// status. Stops on the typed boundary, never by inspecting events.
    async fn await_terminal(&mut self, run_id: Uuid) -> String {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut cursor = 0_u64;
            loop {
                let page = self
                    .client
                    .read_events(with_token(
                        ReadRunEventsRequest {
                            schema_version: 1,
                            invocation: Some(self.invocation.clone()),
                            run_id: run_id.to_string(),
                            after_sequence: cursor,
                            limit: 64,
                        },
                        &self.token,
                    ))
                    .await
                    .expect("read events")
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
        .expect("the Run never reached a terminal boundary")
    }
}

/// The first control command proven to *do* something over the network.
#[tokio::test(flavor = "multi_thread")]
async fn a_network_cancel_stops_a_live_run_and_returns_a_receipt() {
    let mut surface = spawn_surface(111).await;
    let run_id = Uuid::now_v7();
    let epoch = surface.submit(run_id).await;
    let command_id = Uuid::now_v7();

    let receipt = surface
        .client
        .control(with_token(
            surface.cancel(run_id, command_id, epoch),
            &surface.token,
        ))
        .await
        .expect("cancel over the invocation surface")
        .into_inner();

    assert_eq!(receipt.run_id, run_id.to_string());
    assert_eq!(receipt.command_id, command_id.to_string());
    assert_eq!(
        receipt.command_digest.len(),
        64,
        "a receipt must carry the digest that binds it to this exact action"
    );

    assert_eq!(
        surface.await_terminal(run_id).await,
        "cancelled",
        "the Run was not actually stopped by the command"
    );
}

/// `runtime.proto` calls `command_id` a durable idempotency key. Retrying is
/// what a network client does when a response is lost, so the second attempt
/// must reach the same decision rather than a second one.
#[tokio::test(flavor = "multi_thread")]
async fn replaying_the_same_command_id_returns_the_same_receipt() {
    let mut surface = spawn_surface(112).await;
    let run_id = Uuid::now_v7();
    let epoch = surface.submit(run_id).await;
    let command_id = Uuid::now_v7();

    let first = surface
        .client
        .control(with_token(
            surface.cancel(run_id, command_id, epoch),
            &surface.token,
        ))
        .await
        .expect("first cancel")
        .into_inner();
    let second = surface
        .client
        .control(with_token(
            surface.cancel(run_id, command_id, epoch),
            &surface.token,
        ))
        .await
        .expect("a replayed command must be accepted, not rejected")
        .into_inner();

    assert_eq!(
        first.command_digest, second.command_digest,
        "a replay produced a different decision"
    );
    assert_eq!(first.command_id, second.command_id);
    assert_eq!(surface.await_terminal(run_id).await, "cancelled");
}

/// The other half of the same claim: an id already bound to one action must not
/// be usable for a different one. Without this, a caller that reused an id --
/// by accident or otherwise -- could have a cancellation silently stand in for
/// a resume, or the reverse.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_command_id_cannot_be_reused_for_a_different_action() {
    let mut surface = spawn_surface(113).await;
    let run_id = Uuid::now_v7();
    let epoch = surface.submit(run_id).await;
    let command_id = Uuid::now_v7();

    surface
        .client
        .control(with_token(
            surface.cancel(run_id, command_id, epoch),
            &surface.token,
        ))
        .await
        .expect("first cancel");

    let mut different = surface.cancel(run_id, command_id, epoch);
    different.action_json = br#"{"type":"resume"}"#.to_vec();

    let status = surface
        .client
        .control(with_token(different, &surface.token))
        .await
        .expect_err("a command id bound to a cancel must not also mean resume");

    assert_eq!(
        status.code(),
        Code::FailedPrecondition,
        "reusing a command id for another action must be refused, and refused as          the caller's own actionable mistake rather than an internal fault"
    );
}
