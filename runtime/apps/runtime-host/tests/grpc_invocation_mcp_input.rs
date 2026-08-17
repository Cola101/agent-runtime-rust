//! A multi-round MCP input decision delivered over the public Runtime surface.
//!
//! The caller is deliberately limited to its operator token, `run_id`, and
//! durable event bytes. It never reads the Runtime state root. The first
//! Runtime disappears while the Run is suspended; a replacement serves the
//! original event, accepts the exact response, and finishes the same Tool call.

use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::grpc::RuntimeInvocationGrpcService;
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalMcpServerConfig, LocalMcpTransportConfig,
    LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig, LocalToolConsent,
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
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";
const ANSWER: &str = "answer after network MCP confirmation";

async fn read_json_request(socket: &mut TcpStream) -> serde_json::Value {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    let (header_end, content_length) = loop {
        let read = socket.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "request closed before headers");
        request.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            let headers = std::str::from_utf8(&request[..header_end]).expect("headers");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                })
                .expect("content length header");
            break (header_end, content_length);
        }
        assert!(request.len() < 512 * 1024, "request is unexpectedly large");
    };
    while request.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.expect("read request body");
        assert!(read > 0, "request closed before body");
        request.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&request[header_end..header_end + content_length]).expect("request JSON")
}

fn tool_call_turn() -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_network_mrtr",
                    "type": "function",
                    "function": {
                        "name": "mcp:modern/confirm_search",
                        "arguments": "{\"query\":\"runtime evidence\"}"
                    }
                }]
            }
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn text_turn() -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{ANSWER}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

async fn spawn_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let handle = tokio::spawn(async move {
        for body in [tool_call_turn(), text_turn()] {
            let (mut socket, _) = listener.accept().await.expect("model request");
            let _ = read_json_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    (endpoint, handle)
}

async fn spawn_mcp_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_json_request(&mut socket).await;
            let result = match request["method"].as_str().unwrap_or_default() {
                "server/discover" => serde_json::json!({
                    "resultType": "complete",
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": {"tools": {}}
                }),
                "tools/list" => serde_json::json!({
                    "resultType": "complete",
                    "tools": [{
                        "name": "confirm_search",
                        "description": "Search only after explicit confirmation",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"query": {"type": "string"}},
                            "required": ["query"]
                        }
                    }]
                }),
                "tools/call" => {
                    observed_calls.fetch_add(1, Ordering::SeqCst);
                    if request["params"].get("inputResponses").is_none() {
                        serde_json::json!({
                            "resultType": "input_required",
                            "requestState": "network-state-byte-exact",
                            "inputRequests": {
                                "confirmation": {
                                    "method": "elicitation/create",
                                    "params": {
                                        "mode": "form",
                                        "message": "Confirm this search",
                                        "requestedSchema": {
                                            "type": "object",
                                            "properties": {"confirmed": {"type": "boolean"}},
                                            "required": ["confirmed"]
                                        }
                                    }
                                }
                            }
                        })
                    } else {
                        assert_eq!(
                            request["params"]["requestState"],
                            "network-state-byte-exact"
                        );
                        assert_eq!(
                            request["params"]["inputResponses"]["confirmation"]["content"]["confirmed"],
                            true
                        );
                        serde_json::json!({
                            "resultType": "complete",
                            "content": [{"type": "text", "text": "confirmed network evidence"}],
                            "isError": false
                        })
                    }
                }
                other => panic!("unexpected MCP method {other}"),
            };
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": result
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    (endpoint, calls, handle)
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
    workspace_root: &Path,
    provider_endpoint: String,
    mcp_endpoint: String,
) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root: state_root.to_path_buf(),
        workspace_root: workspace_root.to_path_buf(),
        agent_instructions: "Use the confirmed evidence before answering.".into(),
        delegated_scopes: BTreeSet::from([
            "tool:mcp:modern".to_owned(),
            "mcp:elicitation:modern".to_owned(),
        ]),
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
        mcp_servers: vec![LocalMcpServerConfig {
            server_id: Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0090),
            name: "modern".into(),
            transport: LocalMcpTransportConfig::StreamableHttp2026 {
                endpoint: mcp_endpoint,
                elicitation: true,
            },
            tool_names: BTreeSet::from(["confirm_search".to_owned()]),
            tool_effect_overrides: BTreeMap::new(),
            required: true,
        }],
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::AllowOnce,
        budget: RunBudget {
            max_tokens: 10_000,
            max_cost_cents: 100,
            max_duration_seconds: 120,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

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

struct PendingInput {
    input_id: String,
    input_version: u64,
    binding_digest: String,
}

fn pending_input_from_events(events: &[RuntimeEvent]) -> Option<PendingInput> {
    events
        .iter()
        .filter(|event| event.r#type == "mcp.input.required")
        .find_map(|event| {
            let payload: serde_json::Value = serde_json::from_slice(&event.payload_json).ok()?;
            let input = payload.get("input")?;
            let request = input.get("requests")?.get("confirmation")?;
            if request.get("mode")?.as_str()? != "form" {
                return None;
            }
            Some(PendingInput {
                input_id: input.get("input_id")?.as_str()?.to_owned(),
                input_version: payload.get("input_version")?.as_u64()?,
                binding_digest: input.get("binding_digest")?.as_str()?.to_owned(),
            })
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_input_is_discovered_and_resolved_over_a_replacement_runtime() {
    let signing_key = SigningKey::from_bytes(&[141; 32]);
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let (provider_endpoint, provider) = spawn_provider().await;
    let (mcp_endpoint, mcp_calls, mcp) = spawn_mcp_server().await;
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

    let first_state = state.path().to_path_buf();
    let first_workspace = workspace_root.clone();
    let first_provider = provider_endpoint.clone();
    let first_mcp = mcp_endpoint.clone();
    let first_invocation = invocation.clone();
    let first_token = token.clone();
    let owner_epoch = tokio::task::spawn_blocking(move || {
        let thread_runtime = tokio::runtime::Runtime::new().unwrap();
        thread_runtime.block_on(async move {
            let signing_key = SigningKey::from_bytes(&[141; 32]);
            let first = spawn_surface(
                &signing_key,
                profile,
                config(&first_state, &first_workspace, first_provider, first_mcp),
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
                        input: "Confirm the MCP search before answering.".into(),
                    },
                    &first_token,
                ))
                .await
                .expect("submit to first Runtime")
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
                        .expect("read first Runtime events")
                        .into_inner();
                    if matches!(
                        page.boundary.and_then(|boundary| boundary.boundary),
                        Some(Boundary::Suspended(_))
                    ) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("Run never suspended for MCP input");
            accepted.owner_epoch
        })
    })
    .await
    .expect("first Runtime thread");

    let second = spawn_surface(
        &signing_key,
        profile,
        config(
            state.path(),
            &workspace_root,
            provider_endpoint,
            mcp_endpoint,
        ),
    )
    .await;
    assert_eq!(
        second
            .runtime
            .recover_unfinished_detached(profile)
            .await
            .expect("reconcile replacement state root"),
        0,
        "a suspended Run must wait for its external response"
    );
    let mut client = RuntimeInvocationClient::connect(format!("http://{}", second.address))
        .await
        .unwrap();

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
        .expect("replacement serves suspended events")
        .into_inner();
    let pending = pending_input_from_events(&page.events)
        .expect("the public event must contain every field needed by ResolveMcpInput");
    assert_eq!(pending.input_version, 1);
    assert_eq!(pending.binding_digest.len(), 64);

    let wrong_version = serde_json::json!({
        "type": "resolve_mcp_input",
        "input_id": pending.input_id,
        "input_version": pending.input_version + 1,
        "binding_digest": pending.binding_digest,
        "responses": {
            "confirmation": {
                "action": "accept",
                "content": {"confirmed": true}
            }
        }
    });
    let wrong_version_error = client
        .control(with_token(
            ControlRunRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                command_id: Uuid::now_v7().to_string(),
                run_id: run_id.to_string(),
                expected_owner_epoch: owner_epoch,
                action_json: serde_json::to_vec(&wrong_version).unwrap(),
            },
            &token,
        ))
        .await
        .expect_err("an unsupported input version must be a caller error");
    assert_eq!(wrong_version_error.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        mcp_calls.load(Ordering::SeqCst),
        1,
        "an invalid response must be rejected before the Tool continuation"
    );
    let unchanged = client
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
        .expect("invalid input version must leave the Run observable")
        .into_inner();
    assert!(matches!(
        unchanged.boundary.and_then(|boundary| boundary.boundary),
        Some(Boundary::Suspended(_))
    ));
    assert!(
        !unchanged
            .events
            .iter()
            .any(|event| event.r#type == "mcp.input.resolved"),
        "an invalid response must not mutate the suspended Run"
    );

    let action = serde_json::json!({
        "type": "resolve_mcp_input",
        "input_id": pending.input_id,
        "input_version": pending.input_version,
        "binding_digest": pending.binding_digest,
        "responses": {
            "confirmation": {
                "action": "accept",
                "content": {"confirmed": true}
            }
        }
    });
    client
        .control(with_token(
            ControlRunRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                command_id: Uuid::now_v7().to_string(),
                run_id: run_id.to_string(),
                expected_owner_epoch: owner_epoch,
                action_json: serde_json::to_vec(&action).unwrap(),
            },
            &token,
        ))
        .await
        .expect("replacement accepts exact MCP input response");

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
                .expect("read replacement events")
                .into_inner();
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
    .expect("resolved MCP Run never reached a terminal boundary");

    assert_eq!(status, "succeeded", "observed {kinds:?}");
    for required in [
        "mcp.input.required",
        "mcp.input.resolved",
        "mcp.input.continuation.started",
        "tool.result",
        "run.succeeded",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == required),
            "missing {required}"
        );
    }
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "run.started").count(),
        1,
        "replacement restarted the Run: {kinds:?}"
    );
    assert!(transcript.contains(ANSWER));
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 2);

    provider.await.expect("provider served both turns");
    mcp.abort();
    let _ = mcp.await;
}
