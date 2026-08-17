//! A Run outliving the Runtime that started it, seen from the network.
//!
//! The surface has been proven against one live Runtime. That is the easy half.
//! The Runtime's whole recovery story -- durable checkpoints, owner epochs,
//! replacement Hosts -- exists so that work survives the process running it,
//! and none of it had been exercised through this surface.
//!
//! So the first Runtime is dropped entirely, along with the server serving it,
//! while a Run is parked on a human decision. A second Runtime opens the same
//! state root, and the caller reconnects with nothing but the `run_id` it was
//! given before the crash. If the surface leaked any in-process handle, the
//! second half of this test could not work.

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
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";
const ANSWER: &str = "answered after the replacement";

const TOOL_CALL_TURN: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
\"id\":\"call_local_1\",\"type\":\"function\",\"function\":{\"name\":\"workspace.read_text\",\
\"arguments\":\"{\\\"path\\\":\\\"README.txt\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";

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
    state_root: &Path,
    workspace_root: PathBuf,
    provider_endpoint: String,
    tool: PathBuf,
) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root: state_root.to_path_buf(),
        workspace_root,
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
                capabilities: BTreeSet::from([Capability::Text, Capability::ToolUse]),
                healthy: true,
                latency_ms: 1,
                cost_per_million_tokens_micros: 1,
                response_timeout_ms: 10_000,
                stream_idle_timeout_ms: 10_000,
            }],
        },
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: Some(tool),
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 10_000,
            max_cost_cents: 100,
            max_duration_seconds: 120,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

/// One Runtime and where it is listening.
///
/// The server task is deliberately not held: the first Runtime is killed by
/// dropping the tokio runtime it lives on, which takes every task with it, and
/// the second lives as long as the test does.
struct Surface {
    runtime: Arc<EmbeddedRuntime>,
    address: SocketAddr,
}

async fn spawn_surface(
    signing_key: &SigningKey,
    profile: RuntimeInvocationContext,
    config: LocalRuntimeConfig,
) -> Surface {
    let runtime = Arc::new(
        EmbeddedRuntime::new(
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
        .unwrap(),
    );
    let service = RuntimeInvocationGrpcService::new(
        Arc::clone(&runtime),
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
    Surface { runtime, address }
}

fn approval_from_events(events: &[RuntimeEvent]) -> Option<(String, String)> {
    events
        .iter()
        .filter(|event| event.r#type == "approval.required")
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
async fn a_run_survives_the_runtime_that_started_it_and_is_finished_over_a_replacement() {
    let tool = trusted_tool_binary();
    let signing_key = SigningKey::from_bytes(&[131; 32]);
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("README.txt"),
        "Agent Runtime native workspace: trusted read-only Tool fixture.\n",
    )
    .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
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

    // ---- the Runtime that starts the work, and then stops existing ----
    //
    // On its own thread with its own tokio runtime, because dropping that
    // runtime aborts every task it spawned. Dropping `Arc`s is not enough: the
    // Run's own detached execution task holds one and is parked on the
    // approval, so the state-root lock stays held and the replacement is
    // refused with "already has another Runtime owner" -- the single-writer
    // guard being right, and the first attempt at this test being wrong.
    // This is the same in-process crash shape `daemon_recovery.rs` uses.
    let first_state = state.path().to_path_buf();
    let first_workspace = workspace_root.clone();
    let first_provider = provider_endpoint.clone();
    let first_tool = tool.clone();
    let first_invocation = invocation.clone();
    let first_token = token.clone();
    let owner_epoch = tokio::task::spawn_blocking(move || {
        let thread_runtime = tokio::runtime::Runtime::new().unwrap();
        thread_runtime.block_on(async move {
            let signing_key = SigningKey::from_bytes(&[131; 32]);
            let first = spawn_surface(
                &signing_key,
                profile,
                config(&first_state, first_workspace, first_provider, first_tool),
            )
            .await;
            let mut client = RuntimeInvocationClient::connect(format!("http://{}", first.address))
                .await
                .unwrap();
            let accepted = client
                .submit(with_token(
                    SubmitRunRequest {
                        invocation: Some(first_invocation.clone()),
                        run_id: run_id.to_string(),
                        input: "Read README.txt.".into(),
                    },
                    &first_token,
                ))
                .await
                .expect("submit to the first Runtime")
                .into_inner();

            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let page = client
                        .read_events(with_token(
                            ReadRunEventsRequest {
                                schema_version: 1,
                                invocation: Some(first_invocation.clone()),
                                run_id: run_id.to_string(),
                                after_sequence: 0,
                                limit: 256,
                            },
                            &first_token,
                        ))
                        .await
                        .expect("read from the first Runtime")
                        .into_inner();
                    if matches!(
                        page.boundary.and_then(|boundary| boundary.boundary),
                        Some(Boundary::WaitingApproval(_))
                    ) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("the Run never parked before the replacement");
            accepted.owner_epoch
        })
        // The thread runtime drops here. The server, the service, the Run's
        // execution task and the state-root lock all go with it.
    })
    .await
    .expect("first Runtime thread");

    // ---- a different Runtime, the same directory, the same run_id ----
    let second = spawn_surface(
        &signing_key,
        profile,
        config(state.path(), workspace_root, provider_endpoint, tool),
    )
    .await;
    let redispatched = second
        .runtime
        .recover_unfinished_detached(profile)
        .await
        .expect("the replacement must reconcile the state root");
    // Zero, and that is the contract rather than a miss: recovery deliberately
    // skips `AwaitingApproval`, because a Run waiting on a human has nothing to
    // redispatch. What releases it is the decision, which is exactly what the
    // rest of this test delivers over the replacement.
    assert_eq!(
        redispatched, 0,
        "a Run parked on a human must not be redispatched by recovery"
    );

    let mut client = RuntimeInvocationClient::connect(format!("http://{}", second.address))
        .await
        .unwrap();

    // The caller carries nothing across the replacement except the run_id it
    // was handed before the crash.
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
        .expect("the replacement must serve the original Run's history")
        .into_inner();

    assert!(
        page.events
            .iter()
            .any(|event| event.r#type == "run.started"),
        "history written by the dead Runtime was lost"
    );
    let (approval_id, binding_digest) =
        approval_from_events(&page.events).expect("the approval must still be discoverable");

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
                // The epoch the first Runtime issued. A replacement that had
                // not taken ownership properly would reject or mis-apply this.
                expected_owner_epoch: owner_epoch,
                action_json: serde_json::to_vec(&action).unwrap(),
            },
            &token,
        ))
        .await
        .expect("a decision made after the replacement must apply");

    let (status, kinds) = tokio::time::timeout(Duration::from_secs(30), async {
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
                .expect("read from the replacement")
                .into_inner();
            let kinds = page
                .events
                .iter()
                .map(|event| event.r#type.clone())
                .collect::<Vec<_>>();
            match page.boundary.and_then(|boundary| boundary.boundary) {
                Some(Boundary::Terminal(terminal)) => return (terminal.status, kinds),
                Some(Boundary::Retired(retired)) => return (retired.status, kinds),
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .expect("the recovered Run never reached a terminal boundary");

    assert_eq!(status, "succeeded", "observed {kinds:?}");
    assert!(
        kinds.iter().any(|kind| kind == "tool.result"),
        "the Tool approved after the replacement did not run; observed {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "run.started").count(),
        1,
        "the replacement restarted the Run instead of resuming it; observed {kinds:?}"
    );
}
