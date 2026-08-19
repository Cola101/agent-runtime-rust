//! Following a Run instead of polling it.
//!
//! `ReadEvents` already lets a caller page to the end. A stream is only worth
//! adding if it does something paging cannot, so these assert the two
//! properties that distinguish it: events arrive *while the Run is still
//! running*, and the stream ends on a typed lifecycle boundary rather than by
//! the caller guessing from the events.
//!
//! The cursor is the same exclusive one `ReadEvents` uses, which is what makes
//! a dropped stream resumable by reconnecting -- so that is asserted too,
//! because a follower that cannot resume is a follower that loses history.

use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::grpc::RuntimeInvocationGrpcService;
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent,
};
use agent_runtime_invocation_protocol::v1::run_event_stream_item::Item;
use agent_runtime_invocation_protocol::v1::run_lifecycle_boundary::Boundary;
use agent_runtime_invocation_protocol::v1::runtime_invocation_client::RuntimeInvocationClient;
use agent_runtime_invocation_protocol::v1::runtime_invocation_server::RuntimeInvocationServer;
use agent_runtime_invocation_protocol::v1::{
    RuntimeInvocationRef, SubmitRunRequest, WatchRunEventsRequest,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::{Channel, Server};
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";
const ANSWER: &str = "streamed while running";

/// Waits for a release before answering, so the test can observe events from a
/// Run that is provably still in flight rather than one that already finished.
async fn spawn_gated_provider() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let (release, released) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut released = Some(released);
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
            if let Some(gate) = released.take() {
                let _ = gate.await;
            }
            let body = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{ANSWER}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    (endpoint, release)
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
    release: tokio::sync::oneshot::Sender<()>,
    _state: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

async fn spawn_surface(seed: u8) -> Surface {
    let signing_key = SigningKey::from_bytes(&[seed; 32]);
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (provider_endpoint, release) = spawn_gated_provider().await;
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
                        response_timeout_ms: 60_000,
                        stream_idle_timeout_ms: 60_000,
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
                    max_duration_seconds: 120,
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
        release,
        _state: state,
        _workspace: workspace,
    }
}

fn watch(invocation: RuntimeInvocationRef, run_id: Uuid, after: u64) -> WatchRunEventsRequest {
    WatchRunEventsRequest {
        schema_version: 1,
        invocation: Some(invocation),
        run_id: run_id.to_string(),
        after_sequence: after,
        capacity: 32,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_sees_a_run_while_it_is_still_running_and_stops_on_a_typed_boundary() {
    let mut surface = spawn_surface(141).await;
    let run_id = Uuid::now_v7();
    surface
        .client
        .submit(with_token(
            SubmitRunRequest {
                invocation: Some(surface.invocation.clone()),
                run_id: run_id.to_string(),
                input: "say something".into(),
            },
            &surface.token,
        ))
        .await
        .expect("submit");

    let mut stream = surface
        .client
        .watch_events(with_token(
            watch(surface.invocation.clone(), run_id, 0),
            &surface.token,
        ))
        .await
        .expect("watch")
        .into_inner();

    // The provider is still gated here, so anything that arrives now arrives
    // from a Run that has not finished. Paging could not tell these apart.
    let mut kinds = Vec::new();
    let mut last_sequence = 0_u64;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(item) = stream.message().await.expect("stream item") {
            match item.item {
                Some(Item::Event(event)) => {
                    last_sequence = event.sequence;
                    kinds.push(event.r#type);
                    if kinds.iter().any(|kind| kind == "run.started") {
                        return;
                    }
                }
                Some(Item::Boundary(_)) | None => {}
            }
        }
    })
    .await
    .expect("no event arrived while the Run was still in flight");

    assert!(
        kinds.iter().any(|kind| kind == "run.started"),
        "observed {kinds:?}"
    );

    // Dropping the stream mid-Run is the ordinary case for a network follower.
    drop(stream);
    let _ = surface.release.send(());

    // Reconnect from the last sequence seen. The cursor is exclusive, so the
    // event already delivered must not arrive twice.
    let mut resumed = surface
        .client
        .watch_events(with_token(
            watch(surface.invocation.clone(), run_id, last_sequence),
            &surface.token,
        ))
        .await
        .expect("resume the watch")
        .into_inner();

    let mut resumed_sequences = Vec::new();
    let terminal = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(item) = resumed.message().await.expect("stream item") {
            match item.item {
                Some(Item::Event(event)) => resumed_sequences.push(event.sequence),
                Some(Item::Boundary(boundary)) => {
                    match boundary.lifecycle.and_then(|lifecycle| lifecycle.boundary) {
                        Some(Boundary::Terminal(terminal)) => return terminal.status,
                        Some(Boundary::Retired(retired)) => return retired.status,
                        _ => {}
                    }
                }
                None => {}
            }
        }
        String::new()
    })
    .await
    .expect("the stream never reported a terminal boundary");

    assert_eq!(terminal, "succeeded", "resumed {resumed_sequences:?}");
    assert!(
        resumed_sequences
            .iter()
            .all(|sequence| *sequence > last_sequence),
        "an exclusive cursor redelivered an event it had already sent: \
         resumed {resumed_sequences:?} after {last_sequence}"
    );
}

/// Streaming authenticates exactly like the unary calls. A surface where only
/// the request/response paths checked would let any caller tail a Run.
#[tokio::test(flavor = "multi_thread")]
async fn watching_without_a_token_is_refused() {
    let mut surface = spawn_surface(142).await;
    let run_id = Uuid::now_v7();

    let refused = match surface
        .client
        .watch_events(watch(surface.invocation.clone(), run_id, 0))
        .await
    {
        Err(status) => status.code(),
        // tonic may accept the call and fail on the first message; either is a
        // refusal, but it must never deliver an item.
        Ok(response) => response
            .into_inner()
            .message()
            .await
            .expect_err("an unauthenticated watch must not deliver events")
            .code(),
    };

    assert_eq!(refused, Code::Unauthenticated);
}

/// The Runtime bounds subscription capacity. Asking for one outside its limits
/// is refused rather than quietly replaced with a different buffer.
#[tokio::test(flavor = "multi_thread")]
async fn an_out_of_range_capacity_is_refused_rather_than_clamped() {
    let mut surface = spawn_surface(143).await;
    let run_id = Uuid::now_v7();
    let mut request = watch(surface.invocation.clone(), run_id, 0);
    request.capacity = 0;

    let status = surface
        .client
        .watch_events(with_token(request, &surface.token))
        .await
        .expect_err("a zero-capacity subscription must be refused");

    assert_ne!(status.code(), Code::Ok);
}
