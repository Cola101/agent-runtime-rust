//! A real Run, driven entirely over the network.
//!
//! The identity tests next door all end in a refusal. Refusals prove the door
//! is shut; they do not prove anything can walk through it. This test is the
//! other half: a caller that holds nothing but a bearer token and a TCP address
//! submits a Run, watches it to a terminal boundary and reads back what the
//! model said -- with no in-process handle to the Runtime at any point.
//!
//! The Provider is a real loopback HTTP/SSE server, so the Run genuinely
//! executes rather than being simulated.

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
    InitializeRuntimeRequest, ReadRunEventsRequest, RuntimeInvocationRef, SubmitRunRequest,
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
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";
const MODEL_REPLY: &str = "the runtime answered over grpc";

async fn spawn_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
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

#[tokio::test(flavor = "multi_thread")]
async fn a_network_caller_submits_observes_and_completes_a_real_run() {
    let signing_key = SigningKey::from_bytes(&[91; 32]);
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let provider_endpoint = spawn_provider().await;

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
            },
        }],
    )
    .expect("runtime");

    let service = RuntimeInvocationGrpcService::new(
        Arc::new(runtime),
        WorkloadTokenVerifier::new(signing_key.verifying_key()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(RuntimeInvocationServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .ok();
    });

    // From here on the caller holds only an address and a token.
    let mut client = RuntimeInvocationClient::connect(format!("http://{address}"))
        .await
        .expect("connect");
    let initialized = client
        .initialize(InitializeRuntimeRequest {
            schema_version: 1,
            min_contract_version: 1,
            max_contract_version: 1,
            required_capabilities: vec!["events.cursor.v1".into(), "run.submit.v1".into()],
        })
        .await
        .expect("initialize")
        .into_inner();
    assert_eq!(initialized.contract_version, 1);
    assert_eq!(initialized.runtime_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(initialized.max_input_bytes, 32_000);
    assert!(
        initialized
            .capabilities
            .iter()
            .any(|capability| capability == "events.watch.v1")
    );
    let invocation = RuntimeInvocationRef {
        schema_version: 1,
        tenant_id: claims.tenant_id.to_string(),
        application_id: claims.application_id.to_string(),
        workload_identity_id: claims.workload_identity_id.to_string(),
        workspace_id: profile.workspace_id.to_string(),
        agent_version_id: profile.agent_version_id.to_string(),
        model_policy_id: profile.model_policy_id.to_string(),
    };
    let run_id = Uuid::now_v7();

    let accepted = client
        .submit(with_token(
            SubmitRunRequest {
                invocation: Some(invocation.clone()),
                run_id: run_id.to_string(),
                input: "say something".into(),
            },
            &token,
        ))
        .await
        .expect("submit")
        .into_inner();
    assert_eq!(accepted.run_id, run_id.to_string());

    // Drain the cursor exactly as an external consumer would: page forward and
    // stop on a typed boundary, never by guessing from the event list.
    let mut cursor = 0_u64;
    let mut seen = Vec::new();
    let terminal_status = tokio::time::timeout(Duration::from_secs(20), async {
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
                .expect("read events")
                .into_inner();

            assert!(
                !page.history_gap,
                "a Run observed from sequence 0 must not report a history gap"
            );
            cursor = page.next_after_sequence;
            seen.extend(page.events.iter().map(|event| event.r#type.clone()));

            match page.boundary.and_then(|boundary| boundary.boundary) {
                Some(Boundary::Terminal(terminal)) => return terminal.status,
                Some(Boundary::Retired(retired)) => return retired.status,
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .expect("the Run did not reach a terminal boundary");

    assert_eq!(
        terminal_status, "succeeded",
        "observed event types: {seen:?}"
    );
    // The whole lifecycle is visible from outside, not just the final answer:
    // acceptance, the routing decision, streamed output, and the terminal fact.
    for expected in [
        "run.started",
        "model.provider.selected",
        "model.output.delta",
        "run.succeeded",
    ] {
        assert!(
            seen.iter().any(|kind| kind == expected),
            "the network caller never saw {expected}: {seen:?}"
        );
    }

    // The transcript is readable over the same surface, so an external consumer
    // does not need a second channel to learn what the model said.
    let replay = client
        .read_events(with_token(
            ReadRunEventsRequest {
                schema_version: 1,
                invocation: Some(invocation),
                run_id: run_id.to_string(),
                after_sequence: 0,
                limit: 256,
            },
            &token,
        ))
        .await
        .expect("replay")
        .into_inner();
    let transcript = replay
        .events
        .iter()
        .map(|event| String::from_utf8_lossy(&event.payload_json).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        transcript.contains(MODEL_REPLY),
        "the model's answer was not readable over the invocation surface"
    );
}
