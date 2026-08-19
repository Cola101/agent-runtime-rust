//! A human decision delivered over the network, and taken from what the
//! network alone could see.
//!
//! Approval is the one control action where the caller cannot invent its
//! arguments: `DecideApproval` needs the `approval_id` and the `binding_digest`
//! of the exact Tool call that was planned, and a decision bound to the wrong
//! call must not apply. So this test never reads the Runtime's state
//! directory. It discovers both fields from the event pages it already has,
//! the way an external console would, and if they were not reachable that way
//! the surface could observe a parked Run and never release it.

use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::grpc::RuntimeInvocationGrpcService;
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use agent_runtime_invocation_protocol::v1::run_lifecycle_boundary::Boundary;
use agent_runtime_invocation_protocol::v1::runtime_invocation_client::RuntimeInvocationClient;
use agent_runtime_invocation_protocol::v1::runtime_invocation_server::RuntimeInvocationServer;
use agent_runtime_invocation_protocol::v1::{
    ControlRunRequest, ReadRunEventsRequest, RuntimeEvent, RuntimeInvocationRef, SubmitRunRequest,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";
const ANSWER: &str = "answered after approval";

const TOOL_CALL_TURN: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
\"id\":\"call_local_1\",\"type\":\"function\",\"function\":{\"name\":\"workspace.read_text\",\
\"arguments\":\"{\\\"path\\\":\\\"README.txt\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";

/// A tool-call turn first, then plain answers, so the Run parks on an approval
/// and can still finish once a decision arrives.
async fn spawn_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    tokio::spawn(async move {
        let mut served = 0_u32;
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
            let body = if served == 0 {
                TOOL_CALL_TURN.to_string()
            } else {
                format!(
                    "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{ANSWER}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
                )
            };
            served += 1;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    endpoint
}

/// Panics rather than skipping. A test that quietly returns when its fixture is
/// missing reports green without having run.
fn trusted_tool_binary() -> PathBuf {
    let mut current = std::env::current_exe().unwrap();
    while current.pop() {
        let candidate = current.join("agent-trusted-workspace-tool");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("agent-trusted-workspace-tool must be built for this test");
}

fn fixture_workspace() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("README.txt"),
        "Agent Runtime native workspace: trusted read-only Tool fixture.\n",
    )
    .unwrap();
    workspace
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

/// Pulls the decision's arguments out of an `approval.required` payload.
///
/// This is the whole point of the test: the caller has event bytes and nothing
/// else. `binding_digest` is what ties the decision to the exact planned Tool
/// call, so a surface that exposed the id without it would still be unable to
/// approve anything.
fn approval_from_events(events: &[RuntimeEvent]) -> Option<(String, String)> {
    events
        .iter()
        .filter(|event| event.r#type == "approval.required")
        // `find_map`, not a loop with `?`: inside a loop the `?` would return
        // None for the whole function on the first unparsable event instead of
        // moving to the next one.
        .find_map(|event| {
            let payload: serde_json::Value = serde_json::from_slice(&event.payload_json).ok()?;
            let approval = payload.get("approval")?;
            Some((
                approval.get("approval_id")?.as_str()?.to_owned(),
                approval
                    .get("execution")?
                    .get("binding_digest")?
                    .as_str()?
                    .to_owned(),
            ))
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_operator_approves_a_parked_run_over_the_network_and_it_finishes() {
    let tool = trusted_tool_binary();
    let signing_key = SigningKey::from_bytes(&[121; 32]);
    let state = tempfile::tempdir().unwrap();
    let workspace = fixture_workspace();
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
                workspace_root: workspace.path().canonicalize().unwrap(),
                agent_instructions: "Explain evidence before conclusions.".into(),
                delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
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
                        // ToolUse, not just Text: this Run registers a Tool, so provider
                        // selection filters for a candidate that can actually call one.
                        capabilities: BTreeSet::from([Capability::Text, Capability::ToolUse]),
                        healthy: true,
                        latency_ms: 1,
                        cost_per_million_tokens_micros: 1,
                        response_timeout_ms: 10_000,
                        stream_idle_timeout_ms: 10_000,
                        max_output_tokens: None,
                    }],
                },
                mcp_servers: Vec::new(),
                mcp_lifecycle: LocalMcpLifecycleConfig::default(),
                trusted_workspace_tool: Some(tool),
                process_session: None,
                // The gate is the point: without it the Tool would run
                // unattended and there would be no decision to deliver.
                consent: LocalToolConsent::Ask,
                budget: RunBudget {
                    max_tokens: 10_000,
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

    let mut client = RuntimeInvocationClient::connect(format!("http://{address}"))
        .await
        .unwrap();
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
                input: "Read README.txt.".into(),
            },
            &token,
        ))
        .await
        .expect("submit")
        .into_inner();

    // Page from zero so the approval event is in hand along with the boundary.
    let (approval_id, binding_digest) = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let page = client
                .read_events(with_token(
                    ReadRunEventsRequest {
                        schema_version: 1,
                        invocation: Some(invocation.clone()),
                        run_id: run_id.to_string(),
                        after_sequence: 0,
                        limit: 256,
                    },
                    &token,
                ))
                .await
                .expect("read events")
                .into_inner();

            if matches!(
                page.boundary.and_then(|boundary| boundary.boundary),
                Some(Boundary::WaitingApproval(_))
            ) && let Some(found) = approval_from_events(&page.events)
            {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the Run never parked on a discoverable approval");

    assert_eq!(
        binding_digest.len(),
        64,
        "a decision must be bound to the exact planned Tool call"
    );

    let action = serde_json::json!({
        "type": "decide_approval",
        "target_run_id": run_id,
        "approval_id": approval_id,
        "binding_digest": binding_digest,
        "decision": "allow_once",
    });
    client
        .control(with_token(
            ControlRunRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                command_id: Uuid::now_v7().to_string(),
                run_id: run_id.to_string(),
                expected_owner_epoch: accepted.owner_epoch,
                action_json: serde_json::to_vec(&action).unwrap(),
            },
            &token,
        ))
        .await
        .expect("the approval decision must be accepted over the network");

    let (status, kinds, transcript) = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let page = client
                .read_events(with_token(
                    ReadRunEventsRequest {
                        schema_version: 1,
                        invocation: Some(invocation.clone()),
                        run_id: run_id.to_string(),
                        after_sequence: 0,
                        limit: 256,
                    },
                    &token,
                ))
                .await
                .expect("read events")
                .into_inner();
            // The event kind lives in its own field, not inside the payload.
            let kinds = page
                .events
                .iter()
                .map(|event| event.r#type.clone())
                .collect::<Vec<_>>();
            let transcript = page
                .events
                .iter()
                .map(|event| String::from_utf8_lossy(&event.payload_json).to_string())
                .collect::<Vec<_>>()
                .join(" ");
            match page.boundary.and_then(|boundary| boundary.boundary) {
                Some(Boundary::Terminal(terminal)) => return (terminal.status, kinds, transcript),
                Some(Boundary::Retired(retired)) => return (retired.status, kinds, transcript),
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .expect("the approved Run never reached a terminal boundary");

    assert_eq!(status, "succeeded");
    assert!(
        kinds.iter().any(|kind| kind == "tool.result"),
        "the approved Tool did not run; observed {kinds:?}"
    );
    assert!(
        transcript.contains(ANSWER),
        "the model did not continue after the Tool result; observed {kinds:?}"
    );
}
